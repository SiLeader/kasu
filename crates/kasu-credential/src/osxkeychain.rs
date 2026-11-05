//! `osxkeychain` helper: generic-password items in the login keychain.

use crate::{Backend, Credential, Error, Result, ServerKey};
use security_framework::{
    item::{ItemClass, ItemSearchOptions, Limit, SearchResult},
    passwords::set_generic_password,
};
use zeroize::Zeroizing;

pub(crate) struct OsxKeychain;

const ERR_ITEM_NOT_FOUND: i32 = -25300;

fn service(server: &ServerKey) -> String {
    format!("kasu:{server}")
}

fn map(e: security_framework::base::Error) -> Error {
    match e.code() {
        -128 | -25293 => Error::Helper {
            name: "osxkeychain".into(),
            message: "access to the keychain was denied or the keychain is locked \
                      (try 'security unlock-keychain')"
                .into(),
        },
        _ => Error::Helper {
            name: "osxkeychain".into(),
            message: format!("keychain error {}", e.code()),
        },
    }
}

fn search(
    service: Option<&str>,
    data: bool,
) -> Result<Vec<std::collections::HashMap<String, String>>> {
    let mut options = ItemSearchOptions::new();
    options
        .class(ItemClass::generic_password())
        .load_attributes(true)
        .load_data(data)
        .limit(Limit::All);
    if let Some(service) = service {
        options.service(service);
    }
    match options.search() {
        Ok(results) => Ok(results
            .iter()
            .filter_map(SearchResult::simplify_dict)
            .filter(|d| d.get("svce").is_some_and(|s| s.starts_with("kasu:")))
            .collect()),
        Err(e) if e.code() == ERR_ITEM_NOT_FOUND => Ok(Vec::new()),
        Err(e) => Err(map(e)),
    }
}

impl Backend for OsxKeychain {
    fn get(&self, server: &ServerKey) -> Result<Option<Credential>> {
        let mut found = search(Some(&service(server)), true)?;
        Ok(found.pop().map(|mut d| {
            let account = d.remove("acct").filter(|a| !a.is_empty() && a != "kasu");
            Credential {
                server: server.clone(),
                account,
                secret: Zeroizing::new(d.remove("v_Data").unwrap_or_default()),
            }
        }))
        .and_then(|c| match c {
            Some(c) if c.secret.is_empty() => Err(Error::Helper {
                name: "osxkeychain".into(),
                message: "the keychain item has no readable secret".into(),
            }),
            other => Ok(other),
        })
    }

    fn store(&self, credential: &Credential) -> Result<()> {
        // Replace rather than update so a changed account name leaves nothing behind.
        self.erase(&credential.server)?;
        set_generic_password(
            &service(&credential.server),
            credential.account.as_deref().unwrap_or("kasu"),
            credential.secret.as_bytes(),
        )
        .map_err(map)
    }

    fn erase(&self, server: &ServerKey) -> Result<bool> {
        let service = service(server);
        let existed = !search(Some(&service), false)?.is_empty();
        if existed {
            ItemSearchOptions::new()
                .class(ItemClass::generic_password())
                .service(&service)
                .delete()
                .map_err(map)?;
        }
        Ok(existed)
    }

    fn list(&self) -> Result<Vec<(ServerKey, Option<String>)>> {
        // Attributes only: reading the data would trigger access prompts.
        Ok(search(None, false)?
            .into_iter()
            .filter_map(|d| {
                let server = ServerKey::parse(d.get("svce")?.strip_prefix("kasu:")?).ok()?;
                let account = d
                    .get("acct")
                    .cloned()
                    .filter(|a| !a.is_empty() && a != "kasu");
                Some((server, account))
            })
            .collect())
    }
}
