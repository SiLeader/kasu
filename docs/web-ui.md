# Web UI

English | [日本語](web-ui_ja.md)

`kasu-web` is the browser interface of kasu: browse repositories, edit files, administer repositories, and review Changeset stacks. It is a separate, stateless process that talks to `kasu-server` over gRPC with a **delegate credential** and states, per request, which account it acts as. kasu-server makes every authorization decision; the Web UI only hides buttons the account cannot use.

Pages are rendered on the server (askama templates, syntect highlighting, sanitized Markdown). A small script adds conveniences (line ranges, copy buttons, relative times, lazy diff loading), but every action also works without JavaScript. There is no Node.js or npm in the build.

## Running

```sh
# kasu-server: accounts and a delegate (see "Server configuration" below)
KASU_ACCOUNTS_FILE=accounts.json KASU_BINDING_KEY_FILE=binding.key \
  cargo run -p kasu-server -- /path/to/kasu-data

# kasu-web
KASU_WEB_SERVER=http://127.0.0.1:50051 \
KASU_WEB_DELEGATE_TOKEN_FILE=secrets/kasu-web.pat \
KASU_WEB_SESSION_KEY_FILE=secrets/web-session.key \
KASU_WEB_PUBLIC_URL=https://kasu.example \
KASU_WEB_CONFIG_FILE=web.json \
  cargo run -p kasu-web
```

| Variable | Meaning |
|---|---|
| `KASU_WEB_LISTEN` | Listen address (default `127.0.0.1:50090`) |
| `KASU_WEB_SERVER` | kasu-server gRPC URL; `https://` uses TLS with the WebPKI roots |
| `KASU_WEB_DELEGATE_TOKEN_FILE` | The delegate token. Omit only for a server without authentication (development): every visitor then shares that unauthenticated access |
| `KASU_WEB_SESSION_KEY_FILE` | HMAC key (32 bytes or more) for session cookies and CSRF tokens; identical on every replica |
| `KASU_WEB_PUBLIC_URL` | External `scheme://host[:port]`. `http://` drops `Secure` and the `__Host-` cookie prefix (development; a warning is logged) |
| `KASU_WEB_METRICS_TOKEN_FILE` | Bearer token for `GET /-/metrics`; metrics are disabled without it |
| `KASU_WEB_CONFIG_FILE` | JSON settings below. Unknown fields are rejected |

```json
{
  "auth": {
    "pat_login": true,
    "proxy": {
      "trusted_peers": ["10.0.0.0/8"],
      "user_header": "X-Forwarded-User",
      "email_header": "X-Forwarded-Email",
      "name_header": "X-Forwarded-Preferred-Username",
      "accounts": { "map": { "alice@example.com": "alice" }, "default_from_user": false }
    },
    "anonymous": { "enabled": true, "account": "public-reader" },
    "login_attempts_per_minute": 10
  },
  "clone_urls": { "http": "https://kasu.example", "ssh": "git@kasu.example:" },
  "default_email_domain": "users.kasu.example",
  "limits": {
    "file_view_bytes": 1048576, "highlight_bytes": 524288, "highlight_lines": 20000,
    "blame_lines": 20000, "diff_files": 100, "diff_lines": 2000, "dir_entries": 5000,
    "upload_bytes": 10485760, "upload_total_bytes": 33554432, "highlight_concurrency": 4
  },
  "cache": { "bytes": 268435456 },
  "session": { "idle_secs": 7200, "max_secs": 43200 }
}
```

`anonymous.account` must name the delegate's `anonymous_account` on the server.

Health endpoints: `GET /-/healthz` (process), `GET /-/readyz` (kasu-server reachable), `GET /-/metrics` (Prometheus; request counts and latency per view and status, upstream errors, logins, cache hits). Repository and account names are never metric labels.

## Server configuration

`KASU_ACCOUNTS_FILE` gains `delegates` and some account fields (details in [repository accounts](repository-accounts.md)):

```json
{
  "accounts": [
    { "name": "alice", "token_file": "alice.pat", "display_name": "Alice", "email": "alice@example.com",
      "create_repositories": ["team-a/*"],
      "grants": [{ "repositories": ["team-a/**"], "write_policy": {}, "admin": true }] },
    { "name": "public-reader", "token_file": "public.pat", "grants": [{ "repositories": ["public/**"] }] }
  ],
  "delegates": [
    { "name": "kasu-web", "token_file": "kasu-web.pat", "act_as": ["*"], "allow_admin": true,
      "allow_unbound": false, "anonymous_account": "public-reader", "trusted_peers": ["10.0.20.0/24"] }
  ]
}
```

Set `KASU_BINDING_KEY_FILE` (32 bytes or more, shared by every server replica) so that Web sessions survive server restarts. Without it each server process uses a random key.

## Signing in

| Method | How the account is chosen | Writes |
|---|---|---|
| PAT login (`/-/login`) | kasu-server validates the PAT with `Identity.WhoAmI`; the session stores the account and the server's *binding* of that PAT, never the PAT | Within the account's permissions |
| Reverse proxy | `user_header` from `trusted_peers` only, mapped by `accounts.map` or (with `default_from_user`) used as the account name | Within the account's permissions; the server delegate needs `allow_unbound` |
| Anonymous | The configured anonymous account | Never: the server fixes anonymous delegation to read-only |

Precedence is proxy header (trusted peers only) → session cookie → anonymous. Headers from untrusted peers are ignored and logged. The operator token, projection accounts and the object hop token cannot sign in.

For TLS-terminating proxies without proxy sign-in, set top-level `trusted_forwarders` to their CIDRs (for example `["127.0.0.1/32"]`). Only these peers, or `auth.proxy.trusted_peers`, may supply the rightmost `X-Forwarded-For` address. This does not trust identity headers. Proxies must overwrite or append the actual client address. Login limits group IPv6 clients by /64 and bound the client map.

Sessions are signed cookies (`__Host-kasu_session`; `Secure; HttpOnly; SameSite=Lax`) that expire after 2 hours of inactivity and 12 hours after sign-in. Logging out deletes the cookie; to end every session of an account immediately, replace its PAT: the binding then no longer matches and kasu-server rejects the session on its next request. Login attempts are rate limited per client address.

## URLs

The first `-` path segment separates the repository name (names never contain a `-` segment) from the view:

```
/                                  repositories           /-/login  /-/logout  /-/preferences  /-/new
/<repo>                            default branch + README
/<repo>/-/tree/<rev>/<path>        directory              /<repo>/-/blob/<rev>/<path>    file
/<repo>/-/raw/<rev>/<path>         raw bytes              /<repo>/-/blame/<rev>/<path>   blame (?moves=1&copies=1)
/<repo>/-/log/<rev>/<path>         history                /<repo>/-/changeset/<id>       changeset (?parent=n)
/<repo>/-/compare/<base>...<head>  three-dot comparison (`..` compares directly)
/<repo>/-/refs                     branches and tags      /<repo>/-/git/<oid>            Git OID → changeset
/<repo>/-/edit|new|upload|delete/<rev>/<path>             Web writes
/<repo>/-/reviews[/new|/<n>[/files|/changesets]]          reviews
/<repo>/-/settings                 administration
```

`<rev>` is one segment: a 64-hex Changeset ID, a full ref name (`refs/...`), or a branch or tag name, with `/` and every reserved byte percent-encoded (`feature%2Fx`). `<path>` is the rest, byte by byte, so non-UTF-8 paths round-trip. Pages shown by a ref link to their Changeset ID permalink (also the `y` key). Signed-in HTML pages use `Cache-Control: private, no-store`. Anonymous pages addressed by Changeset ID use `private, max-age=3600`; other pages use `private, no-cache`.

README, blame, and diff displays check blob sizes before fetching content. Diffs also cap each page at 16 MiB of before/after input and 10,000 rendered rows, including expanded files. Oversized files show a notice instead. Highlighting skips lines over 4 KiB and falls back to plain text when busy. ZIP downloads reject Windows-unsafe paths (backslashes, colons, trailing dots/spaces); use tar.gz for byte-preserving Unix names.

Raw downloads retain the upstream unary file API but reserve their size before fetching, with a shared 256 MiB response budget. The reservation stays held while sending chunks and is released on completion or disconnect; excess concurrent downloads receive a retryable busy response.

## Browsing

- Directory listings (directories first), the latest change of the directory, and its README.
- Files with line numbers and range selection (`#L10-L20`), highlighting within limits, rendered Markdown with a source toggle, inline images (PNG, JPEG, GIF, WebP), and size, executable bit and Git blob OID for others. Symlinks show their target (linked when inside the repository); gitlinks show the submodule commit.
- Blame grouped by origin Changeset, with links to blame the origin's parent.
- History with cursors, merges, and the Git commit OID of imported Changesets.
- Changesets and comparisons with paged file lists, unified or side-by-side diffs (preference), three context lines, and collapsed large files loaded on demand.
- Accounts limited by `read_directories` see a "directory-limited view" badge and nothing outside their directories.
- Each directory entry's latest change (the first 100 entries) is loaded after the page, or shown with `?latest=1` without JavaScript.
- `/<repo>/-/archive/<rev>/<path>` downloads a `tar.gz` (or `?format=zip`) of a directory, streamed from the server's `kasu.v1.Archive` RPC within the caller's read scope (at most 100,000 entries; both total uncompressed file/symlink bytes and archive output are limited to 2 GiB; gitlinks are omitted). The uncompressed size is checked before loading file contents. Downloading an archive does not grant separate Objects API access.
- Short Changeset IDs (4–63 hex digits) are accepted as `<rev>` when no branch or tag has that name and exactly one Changeset matches.

## Writing from the browser

Edit, create, upload and delete files on a branch. The form uploads blobs, creates a Changeset whose parent is the displayed one, then:

1. **lands directly** on the branch (`Land`, which rebases onto a moved tip; conflicts return to the form with the content kept),
2. **creates a branch** and lands there, or
3. **creates a review**, which is the default (and the only choice) when the branch is protected.

Line endings, the final newline and the executable bit are preserved and can be changed; renames are a path change. Each form carries a random idempotency key and its issue time, so a double submission or a retry after a lost response lands once. If the file changed on the branch after the form was opened, the UI asks for confirmation before the server rebases the edit. The refs page creates and deletes branches (`UpdateRef` with the displayed version); force updates and tag replacement are left to the CLI and API.

## Administration

Accounts whose grant has `admin: true` see `/<repo>/-/settings`: default branch, review policy (form for the first rule), webhooks, the whole configuration as JSON, state (Active / ReadOnly / Archived), usage, rename, fork, and deletion (confirmed by typing the name). Only operators can add, change, or remove webhook endpoints; repository admins may preserve existing endpoints while changing other settings. Every change carries the displayed `config_version`; a concurrent change shows the error and the submitted values instead of overwriting. Accounts with `create_repositories` create repositories at `/-/new`, without webhook endpoints.

## Reviews

See [reviews](review.md) for the model and API. The UI lists reviews with approval counts, creates them from a comparison (choosing where the stack starts), and shows the overview (description, submit status with unmet conditions, votes, timeline), files (any revision, or the difference between two revisions; line comments are drafts until published with an optional vote), and the stack's Changesets. Submit (REBASE or SQUASH), *Rebase onto target*, close and reopen are buttons; editing on the review's head can add a new revision.

## Security

- Every response has `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; connect-src 'self'`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: same-origin`, cross-origin isolation headers, and HSTS when the public URL is HTTPS.
- Templates escape by default. Markdown is rendered with pulldown-cmark, raw HTML is shown as text, and the result passes ammonia's allow-list (`http`, `https`, `mailto` and relative links only).
- Raw files are served as `text/plain` or `application/octet-stream` with `Content-Disposition: attachment`, except PNG, JPEG, GIF and WebP, and always with `Content-Security-Policy: sandbox`. SVG, HTML and PDF are never inline.
- State changes are `POST` only, with a CSRF token bound to the session (or proxy user, or a visitor cookie), the form kind and its issue time, plus a same-origin `Origin`/`Referer` check.
- 404 pages are identical whether a repository, ref, path or Changeset is missing or invisible.
- The in-process cache keys every entry by the account the request acted as.

## Verification (2026-10-30)

- **Automated:** in-process kasu-server + kasu-web integration tests (browsing, writes, administration, reviews, authentication, CSRF, XSS payloads, cache isolation, archives), delegation and review tests in kasu-server, a seeded Submit race scenario in kasu-sim, URL round-trip proptests, and the `web_url` / `markdown` fuzz targets with seed replay. The whole workspace passes (311 tests).
- **PostgreSQL 18 (real database):** every PostgreSQL store test, including a review-store test with eight concurrent writers, ref updates sharing the review transaction, idempotent replays and purge. The release `kasu-server` (PostgreSQL backend) and `kasu-web` binaries ran the full flow: PAT sign-in, protecting `main` in the settings, a protected-branch edit routed into a review, approval by another account, and Submit; the ref log records `alice` via `kasu-web:alice`.
- **Browsers with JavaScript disabled:** `tools/webui-browser-check/run.py` deploys the release binaries and drives WebKitGTK 2.52, Chrome 156 and Firefox 158 with page scripts disabled. Each browser sets the language in the preferences, browses anonymously, signs in with a PAT, saves a review policy in the settings, edits a protected file (routed into a review), comments, submits, and sees the landed content and history; all 11 steps pass in all three. With `--page-js` the same run fails at "page scripts did not run" in all three, which confirms the check and that `app.js` runs under the CSP.
- **Fuzzing:** 30-minute libFuzzer campaigns (`cargo +nightly fuzz run`): `markdown` ran 4,954,343 inputs and `web_url` 79,777,670 with no findings. Two earlier `web_url` reports were harness false positives (an encoded `/` producing an empty path or name segment, which the Web UI rejects before use); the property now covers only valid names and paths, and both inputs are kept as seeds.
- **Performance** (release build, 16 cores, SQLite, a generated history of 100,000 changesets over 100 files): import 157 s; warm page latency 4–83 ms (tree, log, path log, file, blame, per-entry history); 100 concurrent visitors issued 1,000 mixed requests with no errors at 1,215 requests/s (p50 74 ms, p95 100 ms, p99 117 ms); highlighting a 500 KiB file took 0.87 s (about 13,700 lines/s), with at most four highlights at once.
- **Cold derived data:** right after an import, the first blame of a file with 1,000 revisions and the first path histories are computed on demand and can exceed the 10-second request budget. The server keeps computing and stores the result; meanwhile the page answers `503` with `Retry-After`, and per-entry history fills what finishes within 5 seconds. Measured on a fresh import: the per-entry view returned in 5.1 s (83 ms once derived), and the first blame was ready about 3 minutes after the import (0.6 s, then 47 ms when warm).

## Browser checklist

The roadmap's manual check covers display, keyboard operation and JavaScript-free operation in the major browsers. `python3 tools/webui-browser-check/run.py` automates every item below except reading the screen with your own eyes; run it after `cargo build --release -p kasu-server -p kasu-web -p kasu-ingest`. `--page-js` is the control and must fail at "page scripts did not run".

| Item | WebKitGTK 2.52 | Chrome 156 | Firefox 158 |
|---|---|---|---|
| Pages render (tree, README, file with highlighting, review) | pass | pass | pass |
| Without JavaScript: preferences, PAT sign-in, settings, edit → review, comment, Submit, history | pass | pass | pass |
| Keyboard: Tab to a listed file and Enter opens it | pass | pass | pass |
| Keyboard: Tab to a line number and Enter selects the line (`#L1`, highlighted row) | pass | pass | pass |
| Keyboard: Tab to a file in a diff list and Enter moves to its diff | pass | pass | pass |
| Visible focus indicator on keyboard focus | pass (computed) | pass (computed) | pass (stylesheet rule; the headless window has no focus) |
| With JavaScript: scripts run under the CSP (deferred per-entry history) | pass | pass | pass |
| Text contrast ≥ 4.5:1 (WCAG 2.1 AA) for every color token pair, light and dark | pass (all 24 pairs; computed from `app.css`) | | |

Safari is not a supported browser; WebKitGTK is checked as a WebKit engine.

## Limits and non-goals

Shown in the table of the configuration above; beyond them files are offered as downloads, highlighting is skipped, and diffs are paged or collapsed. The Web UI does not resolve conflicts, provide code search, issue tracking or notifications, or replace the CLI for force pushes and tag replacement.
