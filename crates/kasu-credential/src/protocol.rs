//! JSON-over-stdio protocol shared by external helpers and `kasu credential`.
//!
//! Requests and responses hold secrets, so neither type implements `Debug`
//! and parse errors never echo the offending input.

use crate::{Credential, Error, Helper, Result, ServerKey};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub const VERSION: u32 = 1;
/// Largest response accepted from a helper.
pub const MAX_RESPONSE: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Operation {
    Get,
    Store,
    Erase,
    List,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Get => "get",
            Operation::Store => "store",
            Operation::Erase => "erase",
            Operation::List => "list",
        }
    }
}

impl std::str::FromStr for Operation {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "get" => Operation::Get,
            "store" => Operation::Store,
            "erase" => Operation::Erase,
            "list" => Operation::List,
            _ => return Err(Error::Config(format!("unknown operation {s:?}"))),
        })
    }
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub server: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Response {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<Entry>>,
}

impl Response {
    pub fn status(status: &str) -> Self {
        Self {
            status: status.into(),
            account: None,
            secret: None,
            entries: None,
        }
    }
}

fn invalid(what: &str) -> Error {
    Error::Config(format!("invalid {what}"))
}

pub fn parse_request(bytes: &[u8]) -> Result<Request> {
    let request: Request = serde_json::from_slice(bytes).map_err(|_| invalid("request"))?;
    if request.version != VERSION {
        return Err(Error::Config(format!(
            "unsupported protocol version {}",
            request.version
        )));
    }
    Ok(request)
}

pub fn parse_response(bytes: &[u8]) -> Result<Response> {
    if bytes.len() > MAX_RESPONSE {
        return Err(Error::Config("response is too large".into()));
    }
    serde_json::from_slice(bytes).map_err(|_| invalid("response"))
}

pub fn encode<T: Serialize>(value: &T) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(serde_json::to_vec(value).expect("protocol types always serialise"))
}

/// Serves one request against `helper`: the implementation of
/// `kasu credential <name> <operation>`.
pub fn handle(helper: &Helper, operation: Operation, input: &[u8]) -> Result<Vec<u8>> {
    let request = parse_request(input)?;
    let server = || {
        request
            .server
            .as_deref()
            .ok_or_else(|| invalid("request"))
            .and_then(ServerKey::parse)
    };
    let response = match operation {
        Operation::Get => match helper.get_blocking(&server()?)? {
            Some(c) => Response {
                account: c.account.clone(),
                secret: Some(c.secret.to_string()),
                ..Response::status("ok")
            },
            None => Response::status("not_found"),
        },
        Operation::Store => {
            let credential = Credential {
                server: server()?,
                account: request.account.clone().filter(|a| !a.is_empty()),
                secret: Zeroizing::new(request.secret.clone().ok_or_else(|| invalid("request"))?),
            };
            helper.store_blocking(&credential)?;
            Response::status("ok")
        }
        Operation::Erase => {
            if helper.erase_blocking(&server()?)? {
                Response::status("ok")
            } else {
                Response::status("not_found")
            }
        }
        Operation::List => match helper.list_blocking() {
            Ok(entries) => Response {
                entries: Some(
                    entries
                        .into_iter()
                        .map(|(server, account)| Entry {
                            server: server.to_string(),
                            account,
                        })
                        .collect(),
                ),
                ..Response::status("ok")
            },
            Err(Error::Unsupported) => Response::status("unsupported"),
            Err(e) => return Err(e),
        },
    };
    let mut out = serde_json::to_vec(&response).map_err(|_| invalid("response"))?;
    out.push(b'\n');
    Ok(out)
}
