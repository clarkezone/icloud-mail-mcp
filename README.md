# iCloud Mail MCP

A Rust service exposing read-only iCloud Mail tools over Streamable HTTP MCP. The assistant reaches the service over a private Tailscale connection; the service connects to iCloud over certificate-validated TLS.

Implemented tools: `list_folders`, `list_messages`, `search_messages`, `get_message`. Reads preserve unread state. Attachment metadata is returned, but attachment contents, sending, flag changes, and folder/message mutations are not implemented.

The implementation has synthetic IMAP wire tests and real HTTP MCP integration tests. Live iCloud/Grokbot acceptance and restricted enrollment lifecycle checks remain required before deployment; see [the specification](docs/spec.md), [review resolutions](docs/review-resolutions.md), and [implementation notes](docs/implementation.md).

## Build and verify

Requires Rust **1.98+**, following the selected MIME parser's minimum version.

```bash
cargo build --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo run --locked -- schemas
```

The `schemas` command prints the exact advertised MCP schemas without connecting to an account. A copy is in [schemas/tools.json](schemas/tools.json). Tests use synthetic messages and a loopback IMAP fixture; production has no plaintext/test-endpoint configuration.

## Configure locally

Create an [Apple app-specific password](https://support.apple.com/en-us/102654). Configure it through the hidden-input prompt; do not pass the password in arguments or chat.

```bash
install -d -m 700 "$HOME/.config/icloud-mail-mcp"
cargo run --locked -- configure --credentials "$HOME/.config/icloud-mail-mcp/credentials.json"
```

The file must be private, owned by the current user, and outside any Git checkout. Setup replaces it atomically. The loader rejects symlink files, non-private parent directories, insecure permissions, oversized files, and invalid credentials. Current permission hardening targets Unix hosts.

An explicit read-only account check is available after the network/identity deployment gate has been verified:

```bash
cargo run --locked -- check --credentials "$HOME/.config/icloud-mail-mcp/credentials.json"
```

The check verifies TLS/login and folder discovery without printing account or folder data. No SMTP or mailbox writes occur. The upstream endpoint is fixed to `imap.mail.me.com:993` using TLS with hostname verification.

## Run the MCP server

Use a spare local port while the connectivity-test server remains on its own port:

```bash
cargo run --locked -- serve --credentials "$HOME/.config/icloud-mail-mcp/credentials.json" --bind 127.0.0.1 --port 8081
curl --fail http://127.0.0.1:8081/healthz
cargo run --locked --example probe -- http://127.0.0.1:8081/mcp
```

Health is process health and never logs into iCloud. The probe discovers all four tools and calls list_folders without printing mailbox data. To expose the service privately, bind the approved Tailscale IP/port and add `--allowed-host` for a hostname used by the client. `--allowed-origin` explicitly permits a browser origin when required; missing Origin works normally. Configure Grokbot with the resulting `/mcp` URL, Streamable HTTP, and no application authentication.

Use a restricted bot enrollment identity from the first connection, including re-enrollment. A personal login with broad grants followed by manual tagging does not pass the deployment gate. Tailnet policy, live identities, addresses, and private verification results stay outside this public repository. No Funnel or service installation is performed by this program.

Strict mode requires the tagged `READ-ONLY` result from `EXAMINE`. For iCloud endpoints that reply with tagged `READ-WRITE`, explicitly add `--allow-icloud-examine-read-write` to `serve`. This is off by default and cannot be enabled by an MCP caller. It tolerates only that selection marker; `EXAMINE`, UID identity checks, the read-command allowlist, and `BODY.PEEK` remain mandatory. No fallback to `SELECT` or write commands is enabled. Compatibility mode relies on the adapter's command restrictions rather than a provider read-only session, and emits a metadata-only startup warning.

Ctrl-C/SIGTERM cancel queued/in-flight reads and close the session without expunging. Restart after changing credentials or deliberately re-arming authentication. A rejected login suppresses additional login attempts for the process lifetime. Restart/account replacement invalidates handles and cursors; snapshots also expire after ten minutes and may be evicted under the cache budget.

Logging is metadata-only. RUST_LOG is ignored so it cannot enable SDK payload dumps or IMAP traces. Provider errors become fixed codes. Credentials, mail bodies/headers, search text, folders, and cursor tokens are not diagnostic fields.

## Public repository boundary

Keep real credentials, account configuration, mailbox contents, and deployment policy outside the checkout. Tests/examples use synthetic data. [Apple settings](https://support.apple.com/en-us/102525) also document SMTP, deferred to a separate phase.
