# Credential Helpers

English | [日本語](credential-helpers_ja.md)

`kasu login` stores a Personal Access Token (PAT) in a **credential helper** so that `clone`, `checkout`, `commit`,
`land`, `review`, `repo` and the other commands authenticate without `--token` or `KASU_TOKEN`. Helpers are pluggable,
as in Git and Docker: built-in ones use the OS secret store, and any executable `kasu-credential-<name>` can be added.
The design is in [docs/plan/credential-helper](plan/credential-helper/README.md) (Japanese only).

```sh
kasu login https://kasu.example.com          # prompts for the PAT without echo
printf '%s' "$PAT" | kasu login https://kasu.example.com --token-stdin --helper pass
kasu repo list                                # no --token needed any more
kasu logout https://kasu.example.com
```

## Token resolution

For each command the token comes from the first of:

1. `--token`
2. `KASU_TOKEN`
3. the credential helper, looked up by the server the command connects to
4. none (an unauthenticated connection)

When 1 or 2 is set, the helper is **never called** (no keychain prompt or D-Bus wait in CI). `KASU_CREDENTIAL_HELPER=none`
disables helper lookups. A helper that fails, or returns a PAT that cannot be sent (for example with surrounding
whitespace), prints a warning and the command continues unauthenticated. If the server answers `UNAUTHENTICATED`, its
error is printed followed by a hint to run `kasu login <server>`. Stored credentials rejected by the server are not
erased automatically.

Credentials are keyed by the normalised server URL: `https://Kasu.Example.com/` becomes `https://kasu.example.com:443`,
paths are dropped, and a unix socket URL such as `unix:///run/kasu/node.sock` is kept as is. The same PAT is used for all
repositories of a server. A stored PAT is only sent to the endpoint it was stored for: when the Objects endpoint
(`--objects`, `KASU_OBJECTS_ENDPOINT`) is a different host or socket, it gets the PAT stored for that endpoint, if any
(`kasu login <objects endpoint>`). An explicit `--token` / `KASU_TOKEN` is sent to both. The PAT is never written to a
working copy.

## Commands

| Command | Behavior |
|---|---|
| `kasu login [SERVER] [--helper H] [--token-stdin] [--no-verify]` | Reads the PAT (no-echo prompt on a terminal, otherwise `--token-stdin`; never from the command line), verifies it with `Identity.WhoAmI`, stores it and records the helper in `config.json`. Without `--helper`, logging in again reuses the recorded helper; when the helper changes, the PAT is removed from the previous one. `--no-verify` stores without asking the server. `--token` / `KASU_TOKEN` are not used as input. Ctrl-C at the prompt cancels without storing anything |
| `kasu logout [SERVER] [--helper H] [--all]` | Erases the stored PAT (idempotent; exit status 0 if nothing was stored). `--all` covers every recorded server; a helper that fails is reported and the others are still processed, only erased servers are removed from `config.json`, and the exit status is non-zero. The PAT stays valid on the server until an administrator revokes it |

`SERVER` defaults to `KASU_SERVER`, then the server of the working copy under `-C`, then `http://127.0.0.1:50051`.
`--json` prints `{"server", "account", "helper"}` after login (never the PAT).

`mount` reads the PAT once at startup and keeps using it; logging out does not affect a running mount.

## Choosing a helper

In order: `--helper` (login/logout only), `KASU_CREDENTIAL_HELPER`, the helper recorded for the server at login,
`credential_helper` in `config.json`, then the platform default (`osxkeychain` on macOS, `secretservice` on Linux).
When the default is unusable (for example no D-Bus session over SSH) kasu **does not fall back to plain text**; it fails
and suggests `--helper pass` or `--helper file`.
For `logout`, an unavailable platform default with no configured helper only prints a warning and succeeds;
an explicitly selected or recorded helper that fails still produces a non-zero exit status.

| Helper | OS | Stored in | Encrypted |
|---|---|---|---|
| `osxkeychain` | macOS | login keychain (generic password, service `kasu:<server>`) | yes |
| `secretservice` | Linux | Secret Service default collection (GNOME Keyring, KeePassXC, KWallet) | yes |
| `pass` | Linux / macOS | `$PASSWORD_STORE_DIR/kasu/<scheme>/<host:port>.gpg` | yes (GPG) |
| `file` | all | `$KASU_CREDENTIALS_FILE` or `<config dir>/credentials.json`, mode 0600 | **no** |

- **osxkeychain**: after the `kasu` binary is replaced, macOS may ask again whether to allow access (as with Git).
  Over SSH run `security unlock-keychain` first. Not compiled or tested on Linux; the macOS CI job exercises it.
- **secretservice**: implemented with libsecret's `secret-tool` (package `libsecret-tools`); the PAT is passed on stdin.
  `secret-tool search` prints secrets, so `list` reads and discards them.
- **pass**: requires an initialised store (`pass init <gpg-id>`). Entry layout: first line the PAT, then `account:` and `server:`.
- **file**: writes with 0600 (directory 0700) atomically, refuses to follow symlinks, warns about loose permissions, and
  `kasu login` always warns that the PAT is stored in plain text.

## Configuration

`$KASU_CONFIG_DIR/config.json` (default `$XDG_CONFIG_HOME/kasu/config.json`, else `~/.config/kasu/config.json`). It never
contains secrets.

```json
{
  "version": 1,
  "credential_helper": "pass",
  "helper_timeout_secs": 60,
  "servers": { "https://kasu.example.com:443": { "credential_helper": "pass" } },
  "helpers": { "vault": { "path": "/opt/kasu/kasu-credential-vault" } }
}
```

A file with an unknown `version` is read-only: `login` / `logout` refuse to modify it.

## External helpers

Any name that is not built in runs `kasu-credential-<name> <operation>` (found on `PATH`, or via `helpers.<name>.path`).
Names match `[a-z0-9][a-z0-9-]{0,63}`; built-in names always win. The helper inherits the environment, receives one JSON
request on stdin and writes one JSON response on stdout. A non-zero exit status, invalid JSON, more than 64 KiB of
output, or no answer within `helper_timeout_secs` (default 60) is an error; the PAT is scrubbed from error messages.
Secrets are never passed in arguments or the environment.

| Operation | Request | Response (exit status 0) |
|---|---|---|
| `get` | `{"version":1,"server":"https://kasu.example.com:443"}` | `{"status":"ok","account":"alice","secret":"..."}` or `{"status":"not_found"}` |
| `store` | `{"version":1,"server":"...","account":"alice","secret":"..."}` | `{"status":"ok"}` |
| `erase` | `{"version":1,"server":"..."}` | `{"status":"ok"}` or `{"status":"not_found"}` |
| `list` | `{"version":1}` | `{"status":"ok","entries":[{"server":"...","account":"alice"}]}` or `{"status":"unsupported"}` |

`kasu credential <builtin> get|store|erase|list` (hidden) serves the same protocol with a built-in helper, which is
handy for comparing an external implementation.

## Troubleshooting

- `secretservice is not available (no D-Bus session)`: there is no desktop session (typical over SSH). Use `--helper pass`
  or `--helper file`, or set `credential_helper` in `config.json`.
- `pass is not initialised`: run `pass init <gpg-id>`.
- Still unauthenticated after `login`: `KASU_TOKEN` or `--token` takes precedence; unset it.
- Tests must not touch real stores: set `KASU_CONFIG_DIR`, `KASU_CREDENTIALS_FILE`, `PASSWORD_STORE_DIR` and use
  `dbus-run-session` with a temporary `XDG_DATA_HOME`.
