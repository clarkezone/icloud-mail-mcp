# Implementation and verification notes

## Module boundaries

- server: four typed MCP tools, generated schemas, Host/Origin checks, admission deadline and request-size limit.
- worker: serialized mailbox worker, bounded queue, cancellation/deadlines, retry/backoff and authentication breaker.
- wire: TLS-only production connector, framing guards ahead of imap-proto, internal read-only command allowlist. Plaintext is compiled only under cfg(test).
- mail: EXAMINE/UID operations, modified UTF-7 folders, bounded headers, MIME selection, bounded transfer/charset/HTML decoding.
- state: authenticated handles/cursors and bounded expiring snapshots accounting for String/Vec capacity.
- credentials: hidden local setup, ownership/mode checks, no-follow opening, atomic replacement and zeroized secrets. Paths inside Git checkouts are refused.
- sending: optional dedicated worker thread, private fixed sender, SQLite idempotency/outcome journal and typed status lookup.
- smtp: bounded fixed-endpoint STARTTLS transport, authenticated submission and phase-aware uncertain outcomes; test-only fixture connectors.
- logging: immutable metadata-target allowlist; environment settings cannot enable dependency payloads.

The candidate async-imap wrapper was not used: framing limits and READ-ONLY selection verification must be controlled before general parsing. This client uses imap-proto after its framing guard. General protocol nesting is bounded before parsing; MIME semantic depth/parts are validated before body selection. TLS validates the fixed iCloud hostname against trusted roots.

Only the selected text section is fetched, in partial reads of at most 64 KiB. Complete-message and attachment fetches are absent. Transfer decoding is bounded by encoded size; charset decoding stops at its preallocated output limit. HTML conversion is an offline bounded tokenizer: scripts/head/style/template contents are omitted and no URL is fetched. Unsupported/malformed encodings produce fixed errors or explicit replacement warnings. Complex layout is represented as text, not browser rendering.

Search/list membership is captured once in descending UID order. Later pages use those UIDs with live flags. Body chunks come from one immutable decoded snapshot. Entries expire from creation time and are invalid after restart, eviction, or account replacement. Cached bodies revalidate mailbox validity and message existence.

Configuration changes take effect by restarting the process, establishing fresh signing state and clearing the auth breaker. There is no remote credential/reload tool or file watcher. One bounded decoding task may finish already-started synchronous work after cancellation; it retains its slot, discards the result, and performs no further I/O or cache insertion.

## Automated coverage

Tests run synthetic IMAP commands over TCP and actual MCP over HTTP. They cover read-only operations/unread state, attachment non-retrieval, Unicode folder names, fixed membership/live flags, stale/deleted mail, signed handles, cache bounds/expiry, body chunks/JSON escaping, framing limits, decoder expansion, credentials, logging exclusions, authentication suppression and cancellation.

These controlled-fixture checks do not establish live provider/deployment acceptance. Provider checks are performed separately with explicit local authorization. Private account data and deployment results remain outside this public source tree; restricted initial/replacement identity still needs the controlled acceptance procedure in the spec.

## Explicit iCloud EXAMINE compatibility

Strict mode requires a tagged READ-ONLY result and remains the default. The local serve flag --allow-icloud-examine-read-write enables a separate fixed-endpoint TLS connector that also accepts tagged READ-WRITE from EXAMINE. Missing identity/selection metadata still fails closed. This does not enable SELECT, mailbox mutations, attachment retrieval, or non-PEEK body reads; MCP callers cannot toggle it. The synthetic compatibility test checks body reading without changing unread state, rejected write commands, and continued UIDVALIDITY enforcement. The original strict rejection test is retained.

## Specification refinements

- Rust 1.98 is required by mail-parser 1.0.1; the original scaffold's Rust 1.88 minimum no longer applies.
- get_message includes a warnings array for decoding replacement indicators.
- unsupported_content is a non-retryable error for invalid transfer encoding/unknown charset, avoiding a reconnect loop for bad content.
- Request IDs longer than 256 characters are refused so reflected identifiers cannot bypass the response budget.
- Display headers are clipped with truncated_fields when necessary; metadata_truncated reflects clipping.

Building/testing performs no service installation, deployment, network-policy change, live mailbox mutation or live email delivery.

## Optional outbound phase

See [sending.md](sending.md) for the normative send/status contract, private setup, state retention, transport/worker limits and release evidence. The default remains read-only. Synthetic SMTP and HTTP tests do not establish live delivery or Sent-folder behavior. No live message is sent as part of implementation checks.
