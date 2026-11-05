//! Credential-helper integration: token resolution, `kasu login`/`logout`
//! and the hidden `kasu credential` protocol entry point.

use crate::{Error, Result};
use kasu_credential::{
    BUILTIN_HELPERS, Config, Credential, Helper, ServerKey, Store, protocol, select_helper,
};
use std::{
    io::{IsTerminal, Read, Write},
    sync::Mutex,
};
use zeroize::Zeroizing;

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:50051";

struct Used {
    server: String,
    account: Option<String>,
    from_helper: bool,
}
static LAST: Mutex<Option<Used>> = Mutex::new(None);

fn note(used: Used) {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(used);
}

/// Resolves the token for `server`: `--token`/`KASU_TOKEN` first (the helper is
/// then never consulted), else the configured credential helper.
pub async fn token_for(explicit: Option<&str>, server: &str) -> Option<Zeroizing<String>> {
    if let Some(token) = explicit {
        return Some(Zeroizing::new(token.to_owned()));
    }
    let key = ServerKey::parse(server).ok()?;
    let found = lookup(&key).await;
    note(Used {
        server: key.to_string(),
        account: found.as_ref().and_then(|c| c.account.clone()),
        from_helper: found.is_some(),
    });
    found.map(|c| c.secret.clone())
}

/// Tokens for a remote whose objects endpoint may be a different host.
#[derive(Default)]
pub struct Tokens {
    server: Option<Zeroizing<String>>,
    objects: Option<Zeroizing<String>>,
}

impl Tokens {
    /// The same token for both endpoints (an explicitly given `--token`).
    pub fn shared(token: Option<&str>) -> Self {
        let token = token.map(|t| Zeroizing::new(t.to_owned()));
        Self {
            objects: token.clone(),
            server: token,
        }
    }
    pub fn server(&self) -> Option<&str> {
        self.server.as_deref().map(String::as_str)
    }
    pub fn objects(&self) -> Option<&str> {
        self.objects.as_deref().map(String::as_str)
    }
}

/// Resolves the tokens for `server` and its objects endpoint. An explicit
/// `--token` goes to both; a stored PAT only goes to the endpoint it was
/// stored for, so the objects endpoint gets its own lookup when it differs.
pub async fn tokens_for(explicit: Option<&str>, server: &str, objects: &str) -> Tokens {
    if explicit.is_some() {
        return Tokens::shared(explicit);
    }
    let server_token = token_for(None, server).await;
    let objects_token = match objects_source(server, objects) {
        ObjectsSource::Server => server_token.clone(),
        ObjectsSource::Own(key) => lookup(&key).await.map(|c| c.secret),
        ObjectsSource::Nothing => None,
    };
    Tokens {
        server: server_token,
        objects: objects_token,
    }
}

#[derive(Debug, PartialEq)]
enum ObjectsSource {
    /// The objects endpoint is the server itself.
    Server,
    /// A different endpoint: only a PAT stored for it may be sent there.
    Own(ServerKey),
    /// A different endpoint that cannot have stored credentials.
    Nothing,
}

fn objects_source(server: &str, objects: &str) -> ObjectsSource {
    match (ServerKey::parse(server), ServerKey::parse(objects)) {
        _ if server == objects => ObjectsSource::Server,
        (Ok(a), Ok(b)) if a == b => ObjectsSource::Server,
        (_, Ok(key)) => ObjectsSource::Own(key),
        (_, Err(_)) => ObjectsSource::Nothing,
    }
}

async fn lookup(key: &ServerKey) -> Option<Credential> {
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("kasu: warning: {e}; continuing without credentials");
            return None;
        }
    };
    let name = select_helper(None, Some(key), &config);
    if name == "none" {
        return None;
    }
    // A platform-default helper that cannot run (no D-Bus session on a CI
    // box) must not make commands noisy when nothing was ever stored.
    let configured = std::env::var_os("KASU_CREDENTIAL_HELPER").is_some()
        || config.credential_helper.is_some()
        || config.servers.contains_key(key.as_str());
    let result = async { Helper::open(&name, &config)?.get(key).await }.await;
    match result {
        Ok(Some(found)) => match validate_pat(&found.secret) {
            Ok(()) => Some(found),
            Err(e) => {
                eprintln!(
                    "kasu: warning: the PAT stored in '{name}' for {key} is unusable: {e}; \
                     continuing without credentials"
                );
                None
            }
        },
        Ok(None) => None,
        Err(kasu_credential::Error::NotAvailable(_)) if !configured => None,
        Err(e) => {
            eprintln!(
                "kasu: warning: credential helper '{name}' failed: {e}; continuing without credentials"
            );
            None
        }
    }
}

/// Explains an `UNAUTHENTICATED` failure in terms of `kasu login`. The hint is
/// printed in addition to the error, which may carry the server's reason.
pub fn unauthenticated_hint(error: &Error) -> Option<String> {
    let status = error.downcast_ref::<tonic::Status>()?;
    if status.code() != tonic::Code::Unauthenticated {
        return None;
    }
    let guard = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let used = guard.as_ref()?;
    Some(if used.from_helper {
        let who = used
            .account
            .as_deref()
            .map(|a| format!(" (account {a})"))
            .unwrap_or_default();
        format!(
            "kasu: stored credentials for {}{who} were rejected; run 'kasu login {}' again",
            used.server, used.server
        )
    } else {
        format!(
            "kasu: authentication required; run 'kasu login {}' or set KASU_TOKEN",
            used.server
        )
    })
}

fn validate_pat(pat: &str) -> Result<()> {
    kasu_credential::validate_secret(pat)?;
    kasu_object::bearer(pat)?;
    Ok(())
}

/// Reads a line from the terminal without echo. Signal generation is turned
/// off while reading, so Ctrl-C ends the prompt through this function and the
/// terminal settings are always restored.
fn prompt_secret(prompt: &str) -> Result<Zeroizing<String>> {
    use rustix::termios::{
        LocalModes, OptionalActions, SpecialCodeIndex as C, tcgetattr, tcsetattr,
    };
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    struct Restore(std::fs::File, rustix::termios::Termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = tcsetattr(&self.0, OptionalActions::Flush, &self.1);
        }
    }
    let original = tcgetattr(&tty)?;
    let code = |index| Some(original.special_codes[index]).filter(|c| *c != 0);
    let (intr, eof, erase, kill) = (
        code(C::VINTR),
        code(C::VEOF),
        code(C::VERASE),
        code(C::VKILL),
    );
    let mut silent = original.clone();
    silent
        .local_modes
        .remove(LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG);
    silent.special_codes[C::VMIN] = 1;
    silent.special_codes[C::VTIME] = 0;
    write!(tty, "{prompt}")?;
    tty.flush()?;
    let guard = Restore(tty.try_clone()?, original.clone());
    tcsetattr(&tty, OptionalActions::Flush, &silent)?;
    let mut line = Zeroizing::new(Vec::new());
    let outcome: Result<()> = loop {
        let mut byte = [0u8; 1];
        match tty.read(&mut byte) {
            Ok(0) => break Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => break Err(e.into()),
        }
        let b = byte[0];
        if b == b'\n' || b == b'\r' {
            break Ok(());
        } else if Some(b) == intr {
            break Err("interrupted; nothing was stored".into());
        } else if Some(b) == eof {
            break Ok(());
        } else if Some(b) == erase || b == 0x7f || b == 0x08 {
            // Drop one whole UTF-8 character.
            while line.pop().is_some_and(|c| c & 0xc0 == 0x80) {}
        } else if Some(b) == kill {
            line.clear();
        } else if b >= 0x20 {
            line.push(b);
        }
    };
    drop(guard);
    writeln!(tty)?;
    outcome?;
    let text = std::str::from_utf8(&line).map_err(|_| "the PAT is not valid UTF-8")?;
    Ok(Zeroizing::new(text.to_owned()))
}

fn read_pat(prompt: &str, token_stdin: bool) -> Result<Zeroizing<String>> {
    if token_stdin {
        let mut text = Zeroizing::new(String::new());
        std::io::stdin()
            .lock()
            .take(8192)
            .read_to_string(&mut text)?;
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        return Ok(text);
    }
    if std::io::stdin().is_terminal() {
        return prompt_secret(prompt);
    }
    Err("standard input is not a terminal; pass --token-stdin to read the PAT from it".into())
}

pub struct LoginRequest<'a> {
    pub server: &'a str,
    pub helper: Option<&'a str>,
    pub token_stdin: bool,
    pub no_verify: bool,
    pub json: bool,
    /// `--token`/`KASU_TOKEN` is set (it will take precedence over the stored PAT).
    pub token_overrides: bool,
}

fn open_helper(name: &str, config: &Config, explicit: bool) -> Result<Helper> {
    if name == "none" {
        return Err("credential helpers are disabled (KASU_CREDENTIAL_HELPER=none)".into());
    }
    Helper::open(name, config).map_err(|e| match e {
        kasu_credential::Error::NotAvailable(m) if !explicit && BUILTIN_HELPERS.contains(&name) => {
            format!("{m}; use --helper pass or --helper file").into()
        }
        e => e.into(),
    })
}

pub async fn login(req: LoginRequest<'_>) -> Result<()> {
    let key = ServerKey::parse(req.server)?;
    let config = Config::load()?;
    let name = select_helper(req.helper, Some(&key), &config);
    let previous = config
        .servers
        .get(key.as_str())
        .and_then(|s| s.credential_helper.clone())
        .filter(|p| *p != name && p != "none");
    let helper = open_helper(&name, &config, req.helper.is_some())?;
    let pat = read_pat(&format!("PAT for {key}: "), req.token_stdin)?;
    validate_pat(&pat)?;
    let mut account = None;
    if !req.no_verify {
        match crate::whoami(req.server, &pat).await {
            Ok(principal) if principal.anonymous => {
                return Err("the server treated the PAT as anonymous; it is not valid".into());
            }
            Ok(principal) => account = Some(principal.account).filter(|a| !a.is_empty()),
            Err(e) => {
                return Err(match e.downcast_ref::<tonic::Status>().map(|s| s.code()) {
                    Some(tonic::Code::Unauthenticated) => {
                        format!("the PAT is not valid for {key}; nothing was stored").into()
                    }
                    _ => format!(
                        "cannot verify the PAT with {key}: {e}; nothing was stored \
                         (use --no-verify to store it anyway)"
                    )
                    .into(),
                });
            }
        }
    }
    helper
        .store(&Credential {
            server: key.clone(),
            account: account.clone(),
            secret: pat,
        })
        .await?;
    let recorded = name.clone();
    let server_key = key.to_string();
    tokio::task::spawn_blocking(move || {
        Config::update(|c| {
            c.servers.entry(server_key).or_default().credential_helper = Some(recorded);
        })
    })
    .await??;
    // The PAT moved to another helper; do not leave the old copy behind.
    if let Some(previous) = &previous {
        let erased = async { Helper::open(previous, &config)?.erase(&key).await }.await;
        if let Err(e) = erased {
            eprintln!(
                "kasu: warning: cannot remove the previous PAT for {key} from '{previous}': {e}"
            );
        }
    }
    if req.json {
        println!(
            "{}",
            serde_json::json!({"server": key.as_str(), "account": account, "helper": name})
        );
    } else {
        let who = account
            .as_deref()
            .map(|a| format!(" as {a}"))
            .unwrap_or_default();
        println!("Logged in to {key}{who} (stored in {name}).");
    }
    if name == "file" {
        let path = kasu_credential::FileStore::default_path()?;
        eprintln!(
            "kasu: warning: the PAT was stored in plain text in {}",
            path.display()
        );
    }
    if req.token_overrides {
        eprintln!(
            "kasu: warning: --token/KASU_TOKEN is set and takes precedence; the stored PAT will not be used"
        );
    }
    Ok(())
}

pub struct LogoutRequest<'a> {
    pub server: &'a str,
    pub helper: Option<&'a str>,
    pub all: bool,
    pub json: bool,
}

pub async fn logout(req: LogoutRequest<'_>) -> Result<()> {
    let config = Config::load()?;
    let mut targets: Vec<(ServerKey, String)> = Vec::new();
    let key = ServerKey::parse(req.server)?;
    if req.all {
        for recorded in config.servers.keys() {
            if let Ok(k) = ServerKey::parse(recorded) {
                let name = select_helper(req.helper, Some(&k), &config);
                targets.push((k, name));
            }
        }
        if let Some(name) = req.helper {
            let listed = match open_helper(name, &config, true) {
                Ok(helper) => helper.list().await.map_err(Error::from),
                Err(e) => Err(e),
            };
            match listed {
                Ok(entries) => {
                    for (k, _) in entries {
                        if !targets.iter().any(|(t, n)| *t == k && n == name) {
                            targets.push((k, name.to_owned()));
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.downcast_ref::<kasu_credential::Error>(),
                        Some(kasu_credential::Error::Unsupported)
                    ) => {}
                Err(e) => eprintln!("kasu: warning: cannot list the PATs in '{name}': {e}"),
            }
        }
    } else {
        let name = select_helper(req.helper, Some(&key), &config);
        targets.push((key, name));
    }
    // Each target is attempted; only the ones actually erased are forgotten.
    let mut report = Vec::new();
    let mut failures = Vec::new();
    for (key, name) in targets {
        if name == "none" {
            continue;
        }
        let erased = match open_helper(&name, &config, true) {
            Ok(helper) => helper.erase(&key).await.map_err(Error::from),
            Err(e) => Err(e),
        };
        match erased {
            Ok(removed) => report.push((key, name, removed)),
            Err(e) => failures.push((key, name, e.to_string())),
        }
    }
    let forgotten: Vec<String> = report.iter().map(|(k, ..)| k.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        Config::update(|c| {
            for key in &forgotten {
                c.servers.remove(key);
            }
        })
    })
    .await??;
    for (key, name, error) in &failures {
        eprintln!("kasu: cannot remove the PAT for {key} from '{name}': {error}");
    }
    let outcome = match failures.len() {
        0 => Ok(()),
        n => Err(format!("{n} stored PAT(s) could not be removed").into()),
    };
    if req.json {
        let mut items: Vec<_> = report
            .iter()
            .map(|(k, n, r)| serde_json::json!({"server": k.as_str(), "helper": n, "removed": r}))
            .collect();
        items.extend(failures.iter().map(|(k, n, e)| {
            serde_json::json!({"server": k.as_str(), "helper": n, "removed": false, "error": e})
        }));
        println!("{}", serde_json::Value::Array(items));
        return outcome;
    }
    if report.is_empty() && failures.is_empty() {
        println!("No stored credentials.");
    }
    for (key, name, removed) in &report {
        if *removed {
            println!("Logged out of {key} (removed from {name}).");
        } else {
            println!("No stored credentials for {key}.");
        }
    }
    println!("The PAT itself stays valid on the server; ask an administrator to revoke it.");
    outcome
}

/// `kasu credential <name> <operation>`: one protocol request on stdin, one
/// response on stdout.
pub async fn credential(name: &str, operation: &str) -> Result<()> {
    if !BUILTIN_HELPERS.contains(&name) {
        return Err(format!("{name} is not a built-in credential helper").into());
    }
    let operation: protocol::Operation = operation.parse()?;
    let name = name.to_owned();
    let out = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut input = Zeroizing::new(Vec::new());
        std::io::stdin()
            .lock()
            .take(protocol::MAX_RESPONSE as u64 + 1)
            .read_to_end(&mut input)?;
        let helper = Helper::open(&name, &Config::load()?)?;
        Ok(protocol::handle(&helper, operation, &input)?)
    })
    .await??;
    std::io::stdout().write_all(&out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_pats_only_follow_the_server_they_were_stored_for() {
        let key = |s| ServerKey::parse(s).unwrap();
        assert_eq!(
            objects_source("https://kasu.example.com", "https://kasu.example.com"),
            ObjectsSource::Server
        );
        assert_eq!(
            objects_source("https://kasu.example.com", "https://KASU.example.com:443/"),
            ObjectsSource::Server
        );
        assert_eq!(
            objects_source("https://kasu.example.com", "https://objects.example.net"),
            ObjectsSource::Own(key("https://objects.example.net"))
        );
        assert_eq!(
            objects_source("https://kasu.example.com", "unix:///run/kasu/daemon.sock"),
            ObjectsSource::Own(key("unix:///run/kasu/daemon.sock"))
        );
        assert_eq!(
            objects_source("https://kasu.example.com", "not a url"),
            ObjectsSource::Nothing
        );
    }

    #[test]
    fn an_explicit_token_goes_to_both_endpoints() {
        let tokens = Tokens::shared(Some("tok"));
        assert_eq!(
            (tokens.server(), tokens.objects()),
            (Some("tok"), Some("tok"))
        );
        let none = Tokens::shared(None);
        assert_eq!((none.server(), none.objects()), (None, None));
    }
}
