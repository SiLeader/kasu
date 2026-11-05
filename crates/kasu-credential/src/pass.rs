//! `pass` helper: entries of the `pass` password store under `kasu/`.

use crate::{Backend, Credential, Error, Result, ServerKey};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    io::Write,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    process::{Command, Stdio},
};
use zeroize::Zeroizing;

pub(crate) struct Pass {
    store: PathBuf,
}

/// `kasu/<scheme>/<host:port>`, or `kasu/unix/<base64url(path)>`.
pub(crate) fn entry_name(server: &ServerKey) -> String {
    let (scheme, rest) = server.parts();
    if scheme == "unix" {
        format!("kasu/unix/{}", URL_SAFE_NO_PAD.encode(rest))
    } else {
        format!("kasu/{scheme}/{rest}")
    }
}

/// Inverse of [`entry_name`].
pub(crate) fn server_of(entry: &str) -> Option<ServerKey> {
    let rest = entry.strip_prefix("kasu/")?;
    let (scheme, tail) = rest.split_once('/')?;
    let url = if scheme == "unix" {
        let path = String::from_utf8(URL_SAFE_NO_PAD.decode(tail).ok()?).ok()?;
        format!("unix://{path}")
    } else {
        format!("{scheme}://{tail}")
    };
    let key = ServerKey::parse(&url).ok()?;
    (entry_name(&key) == entry).then_some(key)
}

impl Pass {
    pub(crate) fn new() -> Result<Self> {
        let store = match std::env::var_os("PASSWORD_STORE_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .ok_or_else(|| Error::NotAvailable("HOME is not set".into()))?,
            )
            .join(".password-store"),
        };
        let pass = Self { store };
        pass.require_initialised()?;
        Ok(pass)
    }

    fn require_initialised(&self) -> Result<()> {
        if self.store.join(".gpg-id").is_file() {
            Ok(())
        } else {
            Err(Error::NotAvailable(
                "pass is not initialised; run 'pass init <gpg-id>'".into(),
            ))
        }
    }

    fn file(&self, entry: &str) -> PathBuf {
        self.store.join(format!("{entry}.gpg"))
    }

    fn command(&self, args: &[&str], stdin: Option<&[u8]>, capture: bool) -> Result<Vec<u8>> {
        let mut command = Command::new("pass");
        command.args(args);
        if std::env::var_os("GPG_TTY").is_none()
            && let Ok(tty) = rustix::termios::ttyname(std::io::stdin(), Vec::new())
        {
            command.env("GPG_TTY", std::ffi::OsStr::from_bytes(tty.to_bytes()));
        }
        command
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(if capture {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::inherit());
        let mut child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::NotAvailable("'pass' is not installed".into())
            } else {
                Error::Io(e)
            }
        })?;
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            pipe.write_all(input)?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(Error::Helper {
                name: "pass".into(),
                message: format!("pass {} failed ({})", args[0], output.status),
            });
        }
        Ok(output.stdout)
    }
}

fn parse_entry(server: &ServerKey, text: &str) -> Result<Credential> {
    let mut lines = text.lines();
    let secret = lines.next().unwrap_or_default();
    crate::validate_secret(secret)?;
    let account = lines
        .find_map(|l| l.strip_prefix("account:"))
        .map(|a| a.trim().to_owned())
        .filter(|a| !a.is_empty());
    Ok(Credential {
        server: server.clone(),
        account,
        secret: Zeroizing::new(secret.to_owned()),
    })
}

impl Backend for Pass {
    fn get(&self, server: &ServerKey) -> Result<Option<Credential>> {
        self.require_initialised()?;
        let entry = entry_name(server);
        // Existence is decided by the file so a missing entry never prompts for GPG.
        if !self.file(&entry).is_file() {
            return Ok(None);
        }
        let output = Zeroizing::new(self.command(&["show", &entry], None, true)?);
        let text = Zeroizing::new(String::from_utf8_lossy(&output).into_owned());
        parse_entry(server, &text).map(Some)
    }

    fn store(&self, credential: &Credential) -> Result<()> {
        self.require_initialised()?;
        let entry = entry_name(&credential.server);
        let mut content = Zeroizing::new(format!("{}\n", credential.secret.as_str()));
        if let Some(account) = &credential.account {
            content.push_str(&format!("account: {account}\n"));
        }
        content.push_str(&format!("server: {}\n", credential.server));
        self.command(
            &["insert", "--multiline", "--force", &entry],
            Some(content.as_bytes()),
            false,
        )?;
        Ok(())
    }

    fn erase(&self, server: &ServerKey) -> Result<bool> {
        self.require_initialised()?;
        let entry = entry_name(server);
        if !self.file(&entry).is_file() {
            return Ok(false);
        }
        self.command(&["rm", "--force", &entry], None, false)?;
        Ok(true)
    }

    fn list(&self) -> Result<Vec<(ServerKey, Option<String>)>> {
        self.require_initialised()?;
        let mut found = Vec::new();
        let root = self.store.join("kasu");
        let Ok(schemes) = std::fs::read_dir(&root) else {
            return Ok(found);
        };
        for scheme in schemes.flatten() {
            let Some(scheme_name) = scheme.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(scheme.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(tail) = name.to_str().and_then(|n| n.strip_suffix(".gpg")) else {
                    continue;
                };
                if let Some(key) = server_of(&format!("kasu/{scheme_name}/{tail}")) {
                    found.push((key, None));
                }
            }
        }
        found.sort();
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names_round_trip() {
        for url in [
            "https://kasu.example.com",
            "http://[::1]:50051",
            "http://127.0.0.1:50051",
            "unix:///run/kasu/node.sock",
        ] {
            let key = ServerKey::parse(url).unwrap();
            let entry = entry_name(&key);
            assert!(!entry.contains(".."), "{entry}");
            assert_eq!(server_of(&entry), Some(key));
        }
        assert_eq!(
            entry_name(&ServerKey::parse("https://kasu.example.com").unwrap()),
            "kasu/https/kasu.example.com:443"
        );
    }
}
