//! `secretservice` helper: freedesktop Secret Service, driven through
//! libsecret's `secret-tool`. Secrets are passed on stdin, never as arguments.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::{Backend, Credential, Error, Result, ServerKey};
use std::{
    collections::BTreeMap,
    io::Write,
    process::{Command, Stdio},
};
use zeroize::Zeroizing;

const SCHEMA: &str = "dev.kasu.Credential";

pub(crate) struct SecretService;

impl SecretService {
    pub(crate) fn new() -> Result<Self> {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none_or(|v| v.is_empty()) {
            return Err(Error::NotAvailable(
                "secretservice is not available (no D-Bus session)".into(),
            ));
        }
        Ok(Self)
    }

    /// Runs `secret-tool`. A non-zero exit is a failure, except for `search`
    /// (`allow_silent_failure`), which exits non-zero without a message when
    /// nothing matches.
    fn run(
        &self,
        args: &[&str],
        stdin: Option<&[u8]>,
        allow_silent_failure: bool,
    ) -> Result<(String, Zeroizing<Vec<u8>>)> {
        let mut child = Command::new("secret-tool")
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Error::NotAvailable(
                        "secretservice needs 'secret-tool' (libsecret-tools); or use --helper pass"
                            .into(),
                    )
                } else {
                    Error::Io(e)
                }
            })?;
        if let Some(input) = stdin {
            child.stdin.take().expect("piped stdin").write_all(input)?;
        }
        let output = child.wait_with_output()?;
        let stdout = Zeroizing::new(output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        check_exit(
            output.status.success(),
            output.status.code(),
            &stderr,
            allow_silent_failure,
        )?;
        Ok((stderr, stdout))
    }

    /// Items matching `server` (or all of ours), with their attributes.
    fn search(&self, server: Option<&ServerKey>) -> Result<Vec<Item>> {
        let mut args = vec!["search", "--all", "xdg:schema", SCHEMA];
        if let Some(server) = server {
            args.extend(["kasu.server", server.as_str()]);
        }
        let (stderr, stdout) = self.run(&args, None, true)?;
        let text = Zeroizing::new(String::from_utf8_lossy(&stdout).into_owned());
        Ok(parse_search(&text, &stderr))
    }

    fn clear(&self, attributes: &[&str]) -> Result<()> {
        let mut args = vec!["clear", "xdg:schema", SCHEMA];
        args.extend_from_slice(attributes);
        self.run(&args, None, false)?;
        Ok(())
    }
}

fn check_exit(
    success: bool,
    code: Option<i32>,
    stderr: &str,
    allow_silent_failure: bool,
) -> Result<()> {
    let stderr = stderr.trim();
    if success || (allow_silent_failure && stderr.is_empty()) {
        return Ok(());
    }
    let message = match (stderr.lines().next(), code) {
        (Some(line), _) => line.to_owned(),
        (None, Some(code)) => format!("secret-tool exited with status {code}"),
        (None, None) => "secret-tool was terminated by a signal".to_owned(),
    };
    Err(Error::Helper {
        name: "secretservice".into(),
        message,
    })
}

/// Parses `secret-tool search --all` output. Secrets and metadata come on
/// stdout, one `[path]` block per item; attributes come on stderr in the same
/// item order but, within an item, in hash-table order, with no delimiter.
/// Every item we store carries the same attribute keys, so a key repeating
/// marks the next item. If the grouping does not line up with the items,
/// their attributes are left unknown (`None`).
fn parse_search(stdout: &str, stderr: &str) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    for line in stdout.lines() {
        if line.starts_with('[') && line.ends_with(']') {
            items.push(Item::default());
        } else if let Some(item) = items.last_mut() {
            if let Some(v) = line.strip_prefix("secret = ") {
                item.secret = Some(Zeroizing::new(v.to_owned()));
            } else if let Some(v) = line.strip_prefix("modified = ") {
                item.modified = v.to_owned();
            }
        }
    }
    let mut groups: Vec<BTreeMap<String, String>> = Vec::new();
    let mut current = BTreeMap::new();
    for line in stderr.lines() {
        let Some((key, value)) = line
            .strip_prefix("attribute.")
            .and_then(|l| l.split_once(" = "))
        else {
            continue;
        };
        if current.contains_key(key) {
            groups.push(std::mem::take(&mut current));
        }
        current.insert(key.to_owned(), value.to_owned());
    }
    if !current.is_empty() {
        groups.push(current);
    }
    if groups.len() == items.len() {
        for (item, attributes) in items.iter_mut().zip(groups) {
            item.attributes = Some(attributes);
        }
    }
    items
}

#[derive(Default)]
struct Item {
    secret: Option<Zeroizing<String>>,
    /// `None` when `secret-tool`'s output could not be matched to the item.
    attributes: Option<BTreeMap<String, String>>,
    modified: String,
}

impl Item {
    fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.as_ref()?.get(key).map(String::as_str)
    }
    fn server(&self) -> Option<&str> {
        self.attribute("kasu.server")
    }
    fn account(&self) -> Option<String> {
        self.attribute("kasu.account")
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
    }
}

impl Backend for SecretService {
    fn get(&self, server: &ServerKey) -> Result<Option<Credential>> {
        let mut items = self.search(Some(server))?;
        // The search already filtered on the server; unmatched attributes only
        // cost the account name.
        items.retain(|i| {
            i.secret.is_some() && (i.attributes.is_none() || i.server() == Some(server.as_str()))
        });
        if items.len() > 1 {
            eprintln!("kasu: warning: several secrets stored for {server}; using the newest");
        }
        items.sort_by(|a, b| a.modified.cmp(&b.modified));
        Ok(items.pop().map(|i| Credential {
            server: server.clone(),
            account: i.account(),
            secret: i.secret.expect("filtered"),
        }))
    }

    fn store(&self, credential: &Credential) -> Result<()> {
        let account = credential.account.as_deref().unwrap_or("");
        let previous = self.search(Some(&credential.server))?;
        let label = format!("kasu credentials for {}", credential.server);
        let label_arg = format!("--label={label}");
        // `store` replaces the item with identical attributes. Older items are
        // removed only after it succeeded, so a failure keeps the old secret.
        self.run(
            &[
                "store",
                &label_arg,
                "xdg:schema",
                SCHEMA,
                "kasu.server",
                credential.server.as_str(),
                "kasu.account",
                account,
            ],
            Some(credential.secret.as_bytes()),
            false,
        )?;
        let mut stale: Vec<&str> = previous
            .iter()
            .filter_map(|i| i.attribute("kasu.account"))
            .filter(|a| *a != account)
            .collect();
        stale.sort();
        stale.dedup();
        for old in stale {
            self.clear(&[
                "kasu.server",
                credential.server.as_str(),
                "kasu.account",
                old,
            ])?;
        }
        Ok(())
    }

    fn erase(&self, server: &ServerKey) -> Result<bool> {
        let existed = !self.search(Some(server))?.is_empty();
        if existed {
            self.clear(&["kasu.server", server.as_str()])?;
        }
        Ok(existed)
    }

    fn list(&self) -> Result<Vec<(ServerKey, Option<String>)>> {
        // `secret-tool search` prints secrets; they are dropped (zeroised) unread.
        let items = self.search(None)?;
        if items.iter().any(|i| i.attributes.is_none()) {
            return Err(Error::Helper {
                name: "secretservice".into(),
                message: "unexpected output from secret-tool".into(),
            });
        }
        let mut out: Vec<_> = items
            .iter()
            .filter_map(|i| Some((ServerKey::parse(i.server()?).ok()?, i.account())))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(path: &str, secret: &str, modified: &str) -> String {
        format!(
            "[{path}]\nlabel = kasu\nsecret = {secret}\ncreated = 2026-01-01 00:00:00\nmodified = {modified}\nschema = {SCHEMA}\n"
        )
    }

    #[test]
    fn attributes_are_grouped_whatever_their_order() {
        let stdout = [
            block("/c/1", "tok-a", "2026-01-01 00:00:01"),
            block("/c/2", "tok-b", "2026-01-01 00:00:02"),
            block("/c/3", "tok-c", "2026-01-01 00:00:03"),
        ]
        .concat();
        let stderr = "attribute.kasu.server = https://a:443\n\
                      attribute.kasu.account = alice\n\
                      attribute.kasu.account = bob\n\
                      attribute.kasu.server = https://b:443\n\
                      attribute.kasu.account = \n\
                      attribute.kasu.server = https://c:443\n";
        let items = parse_search(&stdout, stderr);
        let got: Vec<_> = items
            .iter()
            .map(|i| {
                (
                    i.secret.as_deref().unwrap().as_str(),
                    i.server(),
                    i.account(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("tok-a", Some("https://a:443"), Some("alice".to_owned())),
                ("tok-b", Some("https://b:443"), Some("bob".to_owned())),
                ("tok-c", Some("https://c:443"), None),
            ]
        );
    }

    #[test]
    fn unmatched_attributes_are_left_unknown() {
        let stdout = [block("/c/1", "tok-a", "1"), block("/c/2", "tok-b", "2")].concat();
        let items = parse_search(&stdout, "attribute.kasu.server = https://a:443\n");
        assert!(items.iter().all(|i| i.attributes.is_none()));
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn silent_failures_only_pass_for_search() {
        assert!(check_exit(true, Some(0), "", false).is_ok());
        assert!(check_exit(false, Some(1), "", true).is_ok());
        assert!(check_exit(false, Some(1), "  \n", false).is_err());
        assert!(check_exit(false, None, "", false).is_err());
        let Err(Error::Helper { message, .. }) = check_exit(false, Some(1), "locked\nmore", true)
        else {
            panic!("expected a failure")
        };
        assert_eq!(message, "locked");
    }
}
