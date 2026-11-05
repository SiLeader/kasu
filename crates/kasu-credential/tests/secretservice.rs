//! Needs a Secret Service and `secret-tool`. Run inside an isolated session:
//! `dbus-run-session -- sh -c 'echo | gnome-keyring-daemon --unlock >/dev/null;
//!  cargo test -p kasu-credential --test secretservice -- --ignored'`.
mod common;

use kasu_credential::{Config, Helper};

#[test]
#[ignore = "needs a Secret Service on the session bus"]
fn secretservice_helper_round_trips() {
    let helper = Helper::open(
        "secretservice",
        &Config::parse(br#"{"version":1}"#).unwrap(),
    )
    .unwrap();
    common::round_trip(&helper);
}
