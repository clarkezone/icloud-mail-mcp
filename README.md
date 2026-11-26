# iCloud Mail MCP

A Rust service exposing iCloud Mail reads and optional SMTP submission over Streamable HTTP MCP. The assistant reaches the service over a private Tailscale connection; the service connects to iCloud over certificate-validated TLS.

Default tools: `list_folders`, `list_messages`, `search_messages`, `get_message`. Reads preserve unread state. Explicit private sending configuration adds `send_message` and `get_send_status` for new plain-text mail. Attachments, reply threading, flag changes, and folder/message mutations are deferred. See [the sending contract](docs/sending.md).

The implementation has synthetic IMAP wire tests and real HTTP MCP integration tests. It has also been exercised live: Grok Bot listed, searched and read iCloud mail through a posture-restricted tailnet identity, and sent a test message. Remote negative probes of the network gate remain outstanding; see [the specification](docs/spec.md), [review resolutions](docs/review-resolutions.md), and [implementation notes](docs/implementation.md).

## Build and verify

Requires Rust **1.98+**, following the selected MIME parser's minimum version.

```bash
cargo build --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo run --locked -- schemas
cargo run --locked -- schemas --with-sending
```

The `schemas` command prints the exact advertised MCP schemas without connecting to an account. Copies are in [schemas/tools.json](schemas/tools.json) and [schemas/tools-with-sending.json](schemas/tools-with-sending.json). Tests use synthetic messages and a loopback IMAP fixture; production has no plaintext/test-endpoint configuration.

## Configure locally

Create an [Apple app-specific password](https://support.apple.com/en-us/102654). Configure it through the hidden-input prompt; do not pass the password in arguments or chat.

```bash
install -d -m 700 "$HOME/.local/share/icloud-mail-mcp"
cargo run --locked -- configure --credentials "$HOME/.local/share/icloud-mail-mcp/credentials.json"
```

The file must be private, owned by the current user, and outside any Git checkout. Setup replaces it atomically. The loader rejects symlink files, non-private parent directories, insecure permissions, oversized files, and invalid credentials. Current permission hardening targets Unix hosts.

An explicit read-only account check is available after the network/identity deployment gate has been verified:

```bash
cargo run --locked -- check --credentials "$HOME/.local/share/icloud-mail-mcp/credentials.json"
```

The check verifies TLS/login and folder discovery without printing account or folder data. No SMTP or mailbox writes occur. The upstream endpoint is fixed to `imap.mail.me.com:993` using TLS with hostname verification.

## Run the MCP server

Use a spare local port while the connectivity-test server remains on its own port:

```bash
cargo run --locked -- serve --credentials "$HOME/.local/share/icloud-mail-mcp/credentials.json" --bind 127.0.0.1 --port 8081
curl --fail http://127.0.0.1:8081/healthz
cargo run --locked --example probe -- http://127.0.0.1:8081/mcp
```

Health is process health and never logs into iCloud. The probe discovers all four tools and calls list_folders without printing mailbox data. To expose the service privately, bind the approved Tailscale IP/port and add `--allowed-host` for a hostname used by the client. `--allowed-origin` explicitly permits a browser origin when required; missing Origin works normally. Configure Grokbot with the resulting `/mcp` URL, Streamable HTTP, and no application authentication.

Use a restricted bot enrollment identity from the first connection, including re-enrollment. A personal login with broad grants followed by manual tagging does not pass the deployment gate. Live tailnet policy, identities, addresses, and private verification results stay outside this public repository; the examples below use placeholders. No Funnel or service installation is performed by this program.

Strict mode requires the tagged `READ-ONLY` result from `EXAMINE`. For iCloud endpoints that reply with tagged `READ-WRITE`, explicitly add `--allow-icloud-examine-read-write` to `serve`. This is off by default and cannot be enabled by an MCP caller. It tolerates only that selection marker; `EXAMINE`, UID identity checks, the read-command allowlist, and `BODY.PEEK` remain mandatory. No fallback to `SELECT` or write commands is enabled. Compatibility mode relies on the adapter's command restrictions rather than a provider read-only session, and emits a metadata-only startup warning.

Ctrl-C/SIGTERM cancel queued/in-flight reads and close the session without expunging. Restart after changing credentials or deliberately re-arming authentication. A rejected login suppresses additional login attempts for the process lifetime. Restart/account replacement invalidates handles and cursors; snapshots also expire after ten minutes and may be evicted under the cache budget.

Logging is metadata-only. RUST_LOG is ignored so it cannot enable SDK payload dumps or IMAP traces. Provider errors become fixed codes. Credentials, mail bodies/headers, search text, folders, and cursor tokens are not diagnostic fields.

## Restrict Grok Bot on the tailnet

Grok Bot joins the tailnet through a Tailscale OAuth consent that issues a one-hour, single-use auth key. The resulting device is **untagged and owned by the consenting user**, so it inherits every grant that user has. Each reconnect creates a new device (`grokbot-user-<id>`, then `-1`, `-2`, …).

**Do not tag Grok Bot devices.** Tagging one disconnects it immediately and backdates its key expiry ([cursor/plugins#1140](https://github.com/cursor/plugins/issues/1140)). Because the device cannot carry a tag, a tag-based grant never applies to it.

Tailscale sets the device attribute `custom:grokbot` (boolean `true`) on these devices, so a [device posture](https://tailscale.com/kb/1288/device-posture) condition can tell them apart. ACLs are allow-only, so the restriction works by making every existing rule require the *absence* of that attribute, then granting Grok Bot devices only the MCP port:

```jsonc
{
  "postures": {
    "posture:grokbot": ["custom:grokbot == true"],
    "posture:human":   ["custom:grokbot NOT SET"],
  },

  // Applies to every acl/grant that does not set its own srcPosture.
  "defaultSrcPosture": ["posture:human"],

  "grants": [
    // ...existing grants, e.g. {"src": ["user@example.com"], "dst": ["*"], "ip": ["*"]}...
    {
      "src":        ["user@example.com"],
      "srcPosture": ["posture:grokbot"],
      "dst":        ["100.64.0.10"],   // the MCP host's Tailscale IP
      "ip":         ["tcp:8080"],
    },
  ],

  // Put the posture on SSH rules explicitly rather than relying on the default.
  "ssh": [
    {
      "action": "check", "srcPosture": ["posture:human"],
      "src": ["autogroup:member"], "dst": ["autogroup:self"],
      "users": ["autogroup:nonroot", "root"],
    },
  ],

  "tests": [
    {
      "src": "user@example.com", "srcPostureAttrs": {"custom:grokbot": true},
      "proto": "tcp",
      "accept": ["100.64.0.10:8080"],
      "deny":   ["100.64.0.10:22", "100.64.0.10:443", "100.64.0.20:22"],
    },
    {
      "src": "user@example.com", "srcPostureAttrs": {"custom:grokbot": true},
      "proto": "udp", "deny": ["100.64.0.10:8080"],
    },
    // Ordinary devices keep their access.
    {"src": "user@example.com", "proto": "tcp", "accept": ["100.64.0.10:22", "100.64.0.20:22"]},
  ],
}
```

Notes:

- Rules that already set their own `srcPosture` are not covered by `defaultSrcPosture`; review them individually.
- Device posture must be available on the tailnet's plan. Validate before applying: the policy editor, or `POST /api/v2/tailnet/-/acl/validate`, runs the `tests` without saving. Prove the tests honour the attribute by temporarily adding a wrong assertion (a Grok Bot device *accepting* another host) and confirming validation fails.
- `nodeAttrs` targets such as `autogroup:member` are not posture-scoped and still apply to Grok Bot devices; check what they grant.
- After each reconnect, confirm in the device's attributes that the new device is untagged with `custom:grokbot: true`. Then make a mail call through Grok Bot. Policy tests are assertions, not probes: the deployment gate in [docs/spec.md](docs/spec.md) also requires remote probes against known-listening forbidden ports.
- A fresh connection may first reuse a folder handle from before a server restart, which fails without contacting iCloud. Calling `list_folders` again recovers.

## Run as a systemd user service

Install a release build to a stable path, so that later builds in the checkout do not change the running binary:

```bash
cargo build --release --locked
install -D -m 0755 target/release/icloud-mail-mcp "$HOME/.local/bin/icloud-mail-mcp"
```

Create `~/.config/systemd/user/icloud-mail-mcp.service`. Substitute your Tailscale IP (`tailscale ip -4`) and MagicDNS hostname; this unit is private deployment configuration and stays outside the checkout:

```ini
[Unit]
Description=iCloud Mail MCP server (tailnet only)
StartLimitIntervalSec=0

[Service]
Type=exec
# Wait up to two minutes at boot for the Tailscale address before binding to it.
ExecStartPre=/bin/sh -c 'for i in $$(seq 120); do ip -4 -o addr show dev tailscale0 2>/dev/null | grep -q " 100.64.0.10/" && exit 0; sleep 1; done; echo "tailscale0 address not up" >&2; exit 1'
ExecStart=%h/.local/bin/icloud-mail-mcp serve \
  --credentials %h/.local/share/icloud-mail-mcp/credentials.json \
  --sending-config %h/.local/share/icloud-mail-mcp/sending.json \
  --bind 100.64.0.10 \
  --port 8080 \
  --allowed-host mail-host.example-tailnet.ts.net \
  --allow-icloud-examine-read-write
Restart=on-failure
RestartSec=10s
TimeoutStopSec=30s
UMask=0077
NoNewPrivileges=yes

[Install]
WantedBy=default.target
```

Omit `--sending-config` for a read-only deployment. A user service starts at boot only with lingering enabled:

```bash
loginctl enable-linger "$USER"
systemctl --user daemon-reload
systemctl --user enable --now icloud-mail-mcp.service
journalctl --user -u icloud-mail-mcp -f
```

SIGTERM from `systemctl --user stop` cancels in-flight work and closes the IMAP session cleanly. A rejected login suppresses further attempts for the process lifetime and does not exit, so it is not retried by `Restart=on-failure`. After fixing credentials, restart the unit deliberately. To upgrade, rebuild, reinstall the binary, and run `systemctl --user restart icloud-mail-mcp`. Restarting invalidates handles and cursors that clients hold.

## Enable sending locally

Sending is off until a private configuration is explicitly supplied. Create it with hidden prompts; the same app-specific password is reused, and the SMTP login must be a full account address:

```bash
cargo run --locked -- configure-sending --config "$HOME/.local/share/icloud-mail-mcp/sending.json"
```

This initializes the private `send-journal.sqlite3` beside the config. Keep the journal: it prevents a repeated request from producing another submission after a timeout or restart. `serve` refuses missing, empty, corrupt or insecure state; it never silently resets that history. Stop the service before reconfiguring.

```bash
cargo run --locked -- serve --credentials "$HOME/.local/share/icloud-mail-mcp/credentials.json" --sending-config "$HOME/.local/share/icloud-mail-mcp/sending.json" --bind 127.0.0.1 --port 8081 --allow-icloud-examine-read-write
cargo run --locked --example probe -- http://127.0.0.1:8081/mcp --with-sending
```

`send_message` takes a UUIDv4 `idempotency_key`, To/Cc/Bcc address arrays, `subject` and `body_text`. Retain the same key on retries; a changed message needs a new intentional key. Acceptance confirms SMTP submission, not delivery or a Sent copy. If the result is unknown, query `get_send_status` and reconcile any remaining uncertainty manually; do not automatically resend with a new key. The sender comes only from local configuration. Bcc is removed from message headers. Requests remain subject to size/recipient limits in [docs/sending.md](docs/sending.md).

The journal contains IDs/fingerprints/outcomes, not message contents. It is private, capped, account-scoped, and never automatically pruned. Every principal allowed to call this endpoint can send when the option is enabled; retain the restricted Tailscale enrollment/network gates. No live SMTP send is performed by setup, health, schemas, the probe, or synthetic tests. Live delivery remains a separately authorized acceptance check.

## Public repository boundary

Keep real credentials, account configuration, mailbox contents, and deployment policy outside the checkout. Tests/examples use synthetic data. [Apple settings](https://support.apple.com/en-us/102525) document the fixed upstream SMTP endpoint used by the optional sending worker.
