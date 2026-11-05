//! External helpers: `kasu-credential-<name> <operation>` with JSON on stdio.

use crate::{
    Backend, Config, Credential, Error, Result, ServerKey,
    protocol::{self, MAX_RESPONSE, Operation, Request, Response},
};
use std::{
    ffi::OsString,
    io::{Read, Write},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub(crate) struct External {
    name: String,
    program: OsString,
    timeout: Duration,
}

impl External {
    pub(crate) fn new(name: &str, config: &Config) -> Self {
        let program = config
            .helpers
            .get(name)
            .and_then(|h| h.path.clone())
            .map(OsString::from)
            .unwrap_or_else(|| format!("kasu-credential-{name}").into());
        Self {
            name: name.to_owned(),
            program,
            timeout: config.helper_timeout(),
        }
    }

    fn fail(&self, message: impl Into<String>) -> Error {
        Error::Helper {
            name: self.name.clone(),
            message: message.into(),
        }
    }

    fn call(&self, operation: Operation, request: &Request) -> Result<Response> {
        let input = protocol::encode(request);
        let (stdout, stderr) = self.run(operation, &input, request.secret.as_deref())?;
        let response = protocol::parse_response(&stdout);
        // `get` returns the secret, so a helper that logs it must not have it echoed.
        let returned = response.as_ref().ok().and_then(|r| r.secret.as_deref());
        let stderr = scrub(&stderr, &[request.secret.as_deref(), returned]);
        if !stderr.trim().is_empty() {
            eprintln!("{}", stderr.trim_end());
        }
        response.map_err(|e| self.fail(e.to_string()))
    }

    /// Runs the helper; on success returns its stdout and (unscrubbed) stderr.
    fn run(
        &self,
        operation: Operation,
        input: &[u8],
        secret: Option<&str>,
    ) -> Result<(Zeroizing<Vec<u8>>, Zeroizing<String>)> {
        let mut child = Command::new(&self.program)
            .arg(operation.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Error::NotAvailable(format!(
                        "credential helper '{}' not found ({} is not on PATH)",
                        self.name,
                        self.program.to_string_lossy()
                    ))
                } else {
                    self.fail(format!("cannot start: {}", e.kind()))
                }
            })?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let writer_input = Zeroizing::new(input.to_vec());
        std::thread::spawn(move || {
            // A helper that exits early closes the pipe; that is reported by its status.
            let _ = stdin.write_all(&writer_input);
        });

        let oversize = Arc::new(AtomicBool::new(false));
        let (out_tx, out_rx) = mpsc::channel();
        let mut stdout = child.stdout.take().expect("piped stdout");
        let flag = oversize.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let result = (&mut stdout)
                .take(MAX_RESPONSE as u64 + 1)
                .read_to_end(&mut buf);
            if buf.len() > MAX_RESPONSE {
                flag.store(true, Ordering::SeqCst);
            }
            let _ = out_tx.send(result.map(|_| buf));
        });
        let (err_tx, err_rx) = mpsc::channel();
        let mut stderr = child.stderr.take().expect("piped stderr");
        std::thread::spawn(move || {
            // Drain everything so the helper never blocks, keep the first KiB.
            let mut kept = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if kept.len() < 1024 {
                    kept.extend_from_slice(&chunk[..n.min(1024 - kept.len())]);
                }
            }
            let _ = err_tx.send(kept);
        });

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if oversize.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(self.fail("response is too large"));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(self.fail(format!("timed out after {}s", self.timeout.as_secs())));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let grace = Duration::from_secs(2);
        let stdout = out_rx
            .recv_timeout(grace)
            .map_err(|_| self.fail("helper left its output open"))??;
        let stdout = Zeroizing::new(stdout);
        let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
        let stderr = Zeroizing::new(String::from_utf8_lossy(&stderr).into_owned());
        if !status.success() {
            let returned = protocol::parse_response(&stdout)
                .ok()
                .and_then(|r| r.secret);
            let stderr = scrub(&stderr, &[secret, returned.as_deref()]);
            let message = match (status.code(), stderr.trim()) {
                (Some(code), "") => format!("exited with status {code}"),
                (Some(code), text) => format!("exited with status {code}: {text}"),
                (None, _) => "terminated by a signal".to_owned(),
            };
            return Err(self.fail(message));
        }
        if stdout.len() > MAX_RESPONSE {
            return Err(self.fail("response is too large"));
        }
        Ok((stdout, stderr))
    }
}

/// Removes the secrets from text that may be shown to the user.
fn scrub(text: &str, secrets: &[Option<&str>]) -> String {
    secrets
        .iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .fold(text.to_owned(), |text, secret| text.replace(secret, "***"))
}

impl Backend for External {
    fn get(&self, server: &ServerKey) -> Result<Option<Credential>> {
        let response = self.call(
            Operation::Get,
            &Request {
                version: protocol::VERSION,
                server: Some(server.to_string()),
                account: None,
                secret: None,
            },
        )?;
        match response.status.as_str() {
            "not_found" => Ok(None),
            "ok" => {
                let secret = response
                    .secret
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| self.fail("response has no secret"))?;
                Ok(Some(Credential {
                    server: server.clone(),
                    account: response.account.filter(|a| !a.is_empty()),
                    secret: Zeroizing::new(secret),
                }))
            }
            _ => Err(self.fail("unexpected response status")),
        }
    }

    fn store(&self, credential: &Credential) -> Result<()> {
        let response = self.call(
            Operation::Store,
            &Request {
                version: protocol::VERSION,
                server: Some(credential.server.to_string()),
                account: credential.account.clone(),
                secret: Some(credential.secret.to_string()),
            },
        )?;
        match response.status.as_str() {
            "ok" => Ok(()),
            _ => Err(self.fail("unexpected response status")),
        }
    }

    fn erase(&self, server: &ServerKey) -> Result<bool> {
        let response = self.call(
            Operation::Erase,
            &Request {
                version: protocol::VERSION,
                server: Some(server.to_string()),
                account: None,
                secret: None,
            },
        )?;
        match response.status.as_str() {
            "ok" => Ok(true),
            "not_found" => Ok(false),
            _ => Err(self.fail("unexpected response status")),
        }
    }

    fn list(&self) -> Result<Vec<(ServerKey, Option<String>)>> {
        let response = self.call(
            Operation::List,
            &Request {
                version: protocol::VERSION,
                server: None,
                account: None,
                secret: None,
            },
        )?;
        match response.status.as_str() {
            "unsupported" => Err(Error::Unsupported),
            "ok" => response
                .entries
                .unwrap_or_default()
                .into_iter()
                .map(|e| {
                    Ok((
                        ServerKey::parse(&e.server)
                            .map_err(|_| self.fail("invalid server in list"))?,
                        e.account.filter(|a| !a.is_empty()),
                    ))
                })
                .collect(),
            _ => Err(self.fail("unexpected response status")),
        }
    }
}
