# iCloud Mail MCP — Specification

Revision: 0.4, 2026-11-06 UTC. Read-only defaults retained; optional SMTP submission added.

This revision resolves the six findings in [the review resolution record](review-resolutions.md). These are implementation requirements, not claims that the service or deployment has passed its tests.

Optional outbound behavior is defined in [sending.md](sending.md). The read-only limits and guarantees below apply to the IMAP adapter and default mode; enabling the separately configured SMTP worker adds two tools without adding IMAP mutations.

## 1. Objective and scope

Provide a Rust MCP server that connects to one configured iCloud account as an IMAP client. An assistant can discover folders, list and search messages, and read decoded content without changing mail or user-controlled flags.

V1 exposes exactly four mail tools: `list_folders`, `list_messages`, `search_messages`, and `get_message`. Attachment metadata is included; attachment contents, previews during enumeration, moves, copies, flag changes, folder mutations, drafts, permanent deletion, and sending are outside V1. Local-only Apple Mail folders are outside the account's IMAP interface.

The existing echo project validated Streamable HTTP interoperability, including MCP 2025-11-25, with Grokbot. It is a transport reference; live iCloud behavior remains to be tested.

## 2. Architecture

```text
Assistant → private Tailscale endpoint → Streamable HTTP /mcp
          → typed mail operations → bounded IMAP worker
          → certificate-validated TLS → iCloud IMAP
```

Use Rust 2024 (minimum Rust 1.98, required by the selected MIME dependency), Tokio, Axum, and the official `rmcp` SDK. The implementation uses a private bounded framer before `imap-proto`, `tokio-rustls`, and bounded MIME/header decoding. The candidate `async-imap` session wrapper was not selected because framing and READ-ONLY verification need to be controlled before general parsing. See implementation.md for module boundaries; dependency upgrades require re-verifying the framing/logging constraints.

Separate MCP transport, domain types, credential loading, IMAP transport/framing, read-only adapter, MIME decoding, snapshots, and operational logging. One worker owns one authenticated IMAP session. Mailbox selection and commands from different jobs cannot interleave. A separate bounded decoding worker may process already-fetched bytes.

Default development binding is loopback. Deployment explicitly selects the approved Tailscale address and TCP port, normally 8080. Alternatively use loopback behind private Tailscale Serve. Retain Host/Origin validation, support protocol negotiation, and never enable Funnel as part of this service.

There is no application login/API key: Tailscale is the accepted downstream access-control boundary. The upstream iCloud credential remains local. This authorizes mail reads by every principal admitted by the deployment policy; opaque message handles are not a second authorization system.

## 3. Restricted network identity and deployment gate — R3

The assistant must enroll using an identity that is restricted from its first connection, including after token refresh, reset, replacement, or re-enrollment. Choose one verified deployment profile:

1. A supported tagged enrollment flow creates nodes with only the dedicated bot tag, whose grants permit the one target IP and TCP service port.
2. A dedicated account has no broad grants or privileges and only the intended service access. If tags are applied later, its untagged state must have either zero access or only that same narrow access.

Personal-account enrollment followed by manual tagging is not an acceptable production profile when that account has broader grants. Whether Grokbot's integration supports a tagged join flow remains an integration question; do not claim it does without testing. If neither profile is possible, the deployment does not pass the isolation gate until its enrollment/policy arrangement changes. Application authentication is not silently introduced as a substitute.

Before using real mail, validate the effective policy for the initial and replacement identities. Test reconnection and actual node replacement/re-authentication; ordinary connection resumption alone is insufficient. Check for extra tags and additive grants. Keep a user-owned workstation user-owned if existing administration access depends on that identity; an IP-specific destination does not require tagging the workstation.

Network acceptance must show:

- The assistant can reach the configured TCP service endpoint.
- A representative other port on the same node is blocked while a controlled test listener is demonstrably accepting local connections there.
- A representative other node/service is blocked while known to be listening.
- The equivalent checks remain true after enrollment lifecycle tests. Use policy assertions as well as remote probes; a closed-port refusal is not proof of ACL enforcement.

These restrictions concern tailnet service connections. They do not promise to restrict the assistant's unrelated public-internet access, or disable Tailscale coordination/DNS functions. Deployment addresses, identities, full tailnet policies, and live probe results stay outside the public source tree.

## 4. iCloud credentials

Apple documents IMAP at `imap.mail.me.com:993` with TLS. Configure the username explicitly; Apple documents the email-name portion and using the full address if necessary. Use a dedicated, reusable, revocable app-specific password, which requires Apple Account two-factor authentication. It is configured once, not passed as a one-time login code.

A local setup command obtains the login and hidden password input. Store credentials outside the repository in a private directory (0700) and credential file (0600), or equivalent service credential storage. Validate ownership/permissions, avoid symlink replacement, and replace files atomically. Credentials are never command-line arguments, MCP inputs/results, examples, debug fields, or memory notes.

Loading credentials establishes a fresh random account/credential generation. Reloading/changing account configuration or credentials cancels outstanding work, drops the session, invalidates all handles/snapshots, and clears decoded content. Never derive public handles or logging IDs from a password.

An authentication rejection opens a process-wide breaker for that generation. Subsequent mail requests return sanitized `authentication_failed` without another login attempt. Only a deliberate local retry/reload or process restart re-arms authentication; there is no remote tool to reset it. Invalid certificate/hostname validation never falls back to plaintext or insecure TLS. The adapter enforces read-only behavior; the app-specific password itself is not assumed to be read-only.

Sources: [Apple settings](https://support.apple.com/en-us/102525), [app-specific passwords](https://support.apple.com/en-us/102654).

## 5. Common tool contract

Publish JSON input/output schemas from these normative contracts before implementing the handlers. Reject unknown fields. Integers must be integers, not strings/floats. Handles and cursors are opaque, authenticated tokens, at most 2048 bytes, bound to the process/account generation. Invalid tokens never become raw IMAP input. They expire on process restart or account reload.

All four tools advertise `readOnlyHint=true`, `destructiveHint=false`, and `idempotentHint=true`. A successful call has `isError=false`, typed `structuredContent`, and a concise text summary. Failures have `isError=true` and structured `{ "error": { "code": "...", "retryable": false, "retry_after_ms": null } }`. Human error text is fixed/sanitized and never includes caller inputs, upstream response text, mailbox names, or mail content. Protocol/schema-invalid arguments use MCP invalid-params errors. Adapter failures use the typed tool-error envelope.

Define these stable error codes: `configuration_error`, `authentication_failed`, `tls_error`, `upstream_unavailable`, `mailbox_not_found`, `message_not_found`, `stale_reference`, `cursor_expired`, `invalid_cursor`, `unsupported_search`, `unsupported_content`, `size_limit`, `busy`, `deadline_exceeded`, `cancelled`, and `internal_error`. Only busy, transient upstream failure, and a fresh-request deadline may be retryable; the presence of `retryable` does not authorize an automatic loop. `retry_after_ms`, when present, is bounded to 1000–30000. Do not return partial successful pages for failed upstream enumeration or search.

Every page exposes `snapshot_created_at` as UTC RFC3339, `next_cursor` as a token or null, and explicit truncation/omission fields. A null cursor means that snapshot is exhausted, not that arbitrary oversized content was silently discarded. Bound the entire serialized MCP response, including both text and structured content.

## 6. Tool inputs and results

### list_folders

Input fields: `prefix?: string` (literal Unicode prefix, default empty, max 1024 UTF-8 bytes), `include_counts?: boolean` (default false), `page_size?: integer` (1–100, default 50), `cursor?: string`.

Use a bounded LIST with fixed server pattern, then apply the literal prefix locally. User prefix text is not an IMAP wildcard pattern. Snapshot folder names and attributes, ordered by their decoded UTF-8 byte sequence without Unicode normalization. Preserve exact mailbox identity and hierarchy delimiter; INBOX follows IMAP's special naming rules.

Result fields: `folders`, `snapshot_created_at`, `next_cursor`. Each folder has `folder_handle`, `name`, `delimiter: string|null`, `selectable: boolean`, `roles: string[]`, `message_count: integer|null`, `unread_count: integer|null`, and `counts_observed_at: timestamp|null`. Discover roles from advertised attributes, never English-name guesses. Counts are queried only for emitted folders when requested, and are live observations. Unknown/unsupported counts remain null. Folder attributes are the snapshot's values; an externally removed/renamed folder can later yield not-found.

### list_messages

Input fields: `folder_handle: string`, `page_size?: integer` (1–100, default 50), `cursor?: string`.

Result fields: `messages`, `snapshot_created_at`, `skipped_unavailable: integer`, `next_cursor`, `consistency: "uid_membership_snapshot_values_live"`. Each summary has `message_handle`, `from`, `to`, `cc`, `subject: string|null`, `header_date: timestamp|null`, `received_at: timestamp`, `size_bytes: integer`, `flags: string[]`, `has_attachments: boolean|null`, and `truncated_fields: string[]`. Addresses are arrays of `{name: string|null, address: string|null}`; malformed values remain explicitly unknown rather than fabricated. No body preview is fetched in V1. Unknown attachment presence is null.

### search_messages

Input fields: `folder_handle: string`, `filters?: object` (default empty), `page_size?: integer` (1–100, default 50), `cursor?: string`. Output is the same page type as list_messages.

Allowed filter fields, combined with AND:

| Field | Type and semantics |
| --- | --- |
| `from_contains` | Nonempty string; substring in IMAP FROM |
| `to_contains` | Nonempty string; substring in IMAP TO, not an implicit CC/BCC union |
| `subject_contains` | Nonempty string; substring in IMAP SUBJECT |
| `text_contains` | Nonempty string; IMAP TEXT (headers/body as supported by provider); not attachment-content indexing |
| `unread` | Boolean; true maps to UNSEEN, false to SEEN |
| `flagged` | Boolean; true maps to FLAGGED, false to UNFLAGGED |
| `received_on_or_after` | Strict YYYY-MM-DD, inclusive IMAP SINCE |
| `received_before` | Strict YYYY-MM-DD, exclusive IMAP BEFORE |

Each search string is at most 1024 UTF-8 bytes. Omitted filters do not constrain results. Present empty strings, invalid dates, unknown fields, and a lower date bound greater than or equal to the upper bound are invalid params. Date comparison uses the calendar date of IMAP INTERNALDATE, ignoring time and timezone per IMAP semantics; it is not a UTC-instant or Date-header filter. Preserve provider text matching semantics and return `unsupported_search` on unsupported charset/criteria. Do not claim fuzzy search, use regular expressions, accept raw queries, or fall back to downloading whole mailboxes. UTF-8 negotiation and literal escaping are adapter responsibilities.

### get_message

Input fields: `message_handle: string`, `body_chunk_bytes?: integer` (256–65536, default 16384), `body_cursor?: string`.

Result fields: `message_handle`, bounded `headers` using the summary address/date/subject types, `flags`, `attachments`, `metadata_truncated: boolean`, `truncated_fields`, `warnings: string[]` (fixed decoding-warning codes), and `body: { text: string, representation: "plain_text"|"html_to_text"|"none", next_cursor: string|null, snapshot_created_at: timestamp, complete: boolean }`. Each attachment entry has `part_ref`, `filename: string|null`, `media_type: string|null`, and `size_bytes: integer|null`. Attachment bytes are never fetched. `complete` means the complete selected body has been returned across the cursor chain, not that all attachments or alternative bodies were returned.

On continuation, the headers/flags may be current observations; the body is the same immutable decoded snapshot. Validate that the message still exists with the same UIDVALIDITY before serving cached body text. A missing message returns `message_not_found`; changed validity/account generation returns `stale_reference`. Unsupported/no displayable text returns representation none, empty text, and complete true. Oversized selected body yields `size_limit`, not a falsely complete prefix.

## 7. Listing and search snapshots — R5

At the first call, EXAMINE the mailbox and record UIDVALIDITY and UIDNEXT. Capture search membership with one bounded UID SEARCH, constrained to UIDs no higher than UIDNEXT minus one. If UIDNEXT indicates an empty range, return an empty snapshot without issuing a reversed/zero UID range. If required identity metadata is missing, fail rather than guess. The snapshot is the exact returned UID set, sorted numerically descending; this is not date sorting or a transactional snapshot of all message values.

Store that fixed UID set in the bounded cache. Never re-run the predicate to fill subsequent pages: a previously unread match remains a member even if another client marks it read. Fetch current values for the saved UIDs; explicitly report that flags and other mutable values are live. Newly arrived or newly matching messages appear only in a new search. Check UIDVALIDITY before each page; skip vanished UIDs and count them.

Cursors authenticate/bind account generation, snapshot ID, mailbox identity, UIDVALIDITY, normalized filters or prefix/count options, ordering version, next item index, and expiry. Continuation must repeat the same effective folder/filter/options; changes return `invalid_cursor`. Page size can change. Process restart, expiration, or eviction returns `cursor_expired`; never reconstruct membership silently. Folder listing uses its fixed name/attribute snapshot with the same continuation rules.

Advance only past entries emitted, or positively identified as unavailable. If the next entry would exceed the response budget, stop before it and preserve it for the next call. Apply documented field caps so an individual entry can fit; otherwise return size_limit rather than looping or skipping it. No duplicate or omitted UID is introduced by serialization budgeting. Bound per-page UID examinations; if sparse deletions exhaust that work budget, an empty page with an advancing cursor and skipped count is valid.

## 8. Bounded upstream reads and MIME selection — R1

The ordinary complete-message parser example is not a fetch strategy. The adapter must enforce wire limits before a general IMAP/MIME parser allocates an announced literal or recursive structure. A library that cannot enforce these limits is unsuitable without a framing guard or patch. Check announced literal lengths, line/structure budgets, cumulative bytes, and nesting as bytes arrive. On breach, close/discard the stream instead of draining arbitrary data.

Read plan:

1. EXAMINE and verify UIDVALIDITY and the selection result according to section 12.
2. Fetch bounded ENVELOPE, FLAGS, INTERNALDATE, RFC822.SIZE, selected headers, and BODYSTRUCTURE as needed. Do not fetch body bytes to list/search mail or calculate a preview.
3. Select one displayable inline body deterministically. Exclude attachment-disposition parts, any part with a filename, and nested message/rfc822 content. Within multipart/alternative prefer eligible text/plain, otherwise text/html. For multipart/related use its declared root when valid, otherwise its first child. In multipart/mixed traverse in wire order to the first eligible body subtree. Unknown/cyclic/over-limit structure fails or returns no displayable body explicitly, never arbitrary attachment text.
4. Fetch only that section and bounded MIME headers, using UID BODY.PEEK partial reads of at most 64 KiB. Root TEXT is allowed only for a verified single text-part message; multipart TEXT and whole-message BODY.PEEK[] are prohibited.
5. Enforce encoded and decoded limits while transfer-decoding and charset-decoding. Render eligible HTML to text offline, without scripts, remote images/stylesheets/URLs, or link following.

Headers, filenames, and bodies are untrusted data. Malformed charsets/encoding produce explicit decode warnings or a sanitized unsupported-content/size error; do not silently mark corrupted output as exact. The service never executes embedded instructions. This service-side guarantee does not claim to prevent an assistant from acting on email text through its other tools.

## 9. Decoded body snapshots and cursors — R4

Decode the capped selected text part once into an immutable UTF-8 text snapshot. Do not equate a decoded offset with an IMAP source-byte offset. Bound both the working decode buffers and the snapshot cache; do not store raw messages or body snapshots on disk.

A body cursor binds process/account generation, full message identity, selected MIME section, representation/converter version, snapshot ID, next UTF-8 byte offset, and fixed expiry. Return slices on code-point boundaries. Chunk size can change between calls; advance exactly over the bytes actually returned after response-budget reduction. JSON escaping and duplicated MCP content count toward the response cap, so a valid requested chunk may yield fewer decoded bytes.

Chunks concatenated in order must exactly equal the snapshot. No renormalization, re-decoding, or HTML conversion occurs between pages. Replaying a cursor returns the same body chunk for the same requested size and response budget. Deletion, stale identity, cache eviction, expiry, restart, and account reload have the explicit errors defined above; never begin a different representation silently. Requested message handle and cursor identity must agree. Body/header metadata may be separately truncated only with explicit indicators.

## 10. Hard resource budgets

These are enforced maxima, not suggestions to tune away during implementation. Changes require a spec/test update. Limits are UTF-8 bytes unless stated otherwise.

| Resource | Limit |
| --- | --- |
| MCP request / entire serialized response | 64 KiB / 128 KiB |
| Page items | Default 50, maximum 100 |
| Body chunk | Default 16 KiB, 256 B–64 KiB; actual output may shrink to fit JSON budget |
| One announced IMAP literal | 64 KiB, checked before buffering |
| One response line / complete BODYSTRUCTURE | 256 KiB each; check recursive structure during parsing |
| Root header bytes / per-part MIME header bytes | 64 KiB each |
| Encoded selected body / decoded text / HTML conversion output | 8 MiB each |
| Cumulative upstream bytes for one tool request | 12 MiB, including reconnect/retry work |
| MIME depth / parts / encoded nested-message decoding | 32 / 256 / zero |
| One displayed subject or header value | 2048 bytes, with truncation indicator |
| Summary address entries | 50 combined across address lists, with truncation indicator |
| get_message displayed metadata | 32 KiB serialized, with metadata truncation indicator |
| Search/list UID snapshot / folder snapshot | 10000 entries each; fail with size_limit if exceeded, never present partial membership as complete |
| Snapshot/cache total | 32 MiB including bodies, UID sets, folder metadata, and indexes; maximum 32 snapshots |
| Snapshot lifetime | 10 minutes from creation; not extended on access |
| Pending IMAP jobs / active IMAP jobs / decoding jobs | 16 / 1 / 1 |
| UID examinations per page | 1000; preserve cursor progress and skipped count |

Reserve cache capacity before decoding/building a snapshot. Eviction returns cursor_expired to affected callers. Bound in-progress metadata/UID collections and reject before materializing an unlimited SEARCH/LIST response. Oversized strings/metadata must not consume unbounded temporary memory merely because the final display is truncated.

## 11. Deadlines, cancellation, and reconnect — R6

Stamp a monotonic absolute 30-second deadline at HTTP tool admission. Queue wait, connection, authentication, upstream reads, decoding, and response preparation all consume that same budget. Changing protocol request IDs or retrying internally never extends it. Connect/TLS and authentication each have a maximum of 5 seconds, additionally capped by time remaining.

Each job carries its absolute deadline, cancellation token, account generation, and response channel. Reject an overfull queue immediately as busy with bounded retry guidance. Skip canceled/expired jobs before any upstream I/O. Cancellation comes from MCP cancellation, a dropped request/response channel, HTTP disconnect where detectable, or the deadline; the deadline remains the bound if transport disconnect detection is unavailable.

Use these worker transitions:

- Queued cancellation/expiry: remove or skip the job, release its reservation, issue no IMAP command.
- Active I/O cancellation/expiry: close and discard the session immediately; V1 does not attempt an unbounded drain. Cancel its I/O future and release worker state before processing another job.
- Network/protocol/stream-budget failure: discard; never reuse a stream with uncertain framing.
- Decode cancellation: stop cooperative work between chunks. A synchronous parser already running may finish only within its pre-reserved bounded input/worker slot; discard its result and perform no further upstream I/O or cache insertion. Keep the slot reserved until that worker actually exits. Never start unbounded detached parsing tasks or parse large input on Tokio I/O threads.
- Global shutdown/account reload: cancel all jobs, close the session, clear snapshots/secrets as appropriate, and stop/reinitialize separately from one caller's cancellation.

Allow at most one reconnect retry for a transient read failure, within the original deadline and byte budget. Re-EXAMINE and revalidate identity first. Use worker-wide reconnect backoff (1, 2, 4, up to 30 seconds with bounded jitter), interruptible by cancellation; return retryable failure if the deadline cannot accommodate it. Certificate failures and rejected credentials are not transient. The authentication breaker in section 4 applies across requests, not only within one tool call.

## 12. Read-only boundary

EXAMINE is the only mailbox-selection operation. Strict mode requires its tagged READ-ONLY result and is the default; fail closed rather than falling back to SELECT. An explicit local serve flag, --allow-icloud-examine-read-write, may tolerate a tagged READ-WRITE result from the fixed certificate-validated iCloud endpoint. It does not accept missing selection markers or UID identity metadata, enable SELECT, or loosen the command/PEEK restrictions. It cannot be enabled through MCP. This opt-in relies on the adapter's non-mutating command boundary instead of server-side read-only selection and emits a static startup warning. Controlled acceptance must confirm unread preservation in this mode. Use UID references and BODY.PEEK, never changing sequence-number identities. Identity combines configured account generation, exact mailbox identity, UIDVALIDITY, and UID. Message-ID headers do not substitute for it.

The private adapter exposes only required authentication/session negotiation, CAPABILITY/LIST/STATUS, EXAMINE, UID SEARCH, safe UID FETCH forms, NOOP, and LOGOUT. Session-only capability negotiation may be allowed through audited typed methods. No STORE, APPEND, MOVE, COPY, EXPUNGE, CLOSE, mailbox/subscription mutation, SMTP through the IMAP adapter, or generic raw-command tool. Cleanup must not call a convenience method that expunges mail. An IMAP socket is inaccessible to MCP handlers and MIME decoders.

The guarantee is that this service does not change mail or its user-controlled flags. Other clients/provider behavior can change a mailbox concurrently. Local cached-body reads still require current account/UIDVALIDITY/existence validation before returning data.

## 13. Logging that cannot expose payloads — R2

At every supported verbosity, logs contain only explicitly allowlisted operational fields: random operation ID, tool identifier, numeric counts, durations, status/error codes, fixed protocol-stage labels, metadata-presence booleans, and process/startup identity. Do not log mail headers/bodies, names/addresses, folders, search text, handles/cursors, credentials, raw requests/results, or raw provider responses. Authentication and provider errors are converted to fixed codes before any formatting.

Install mandatory filtering on every subscriber/output: disable unaudited dependency events, including rmcp request/result/error payload events and IMAP protocol traces, at every level. RUST_LOG or CLI diagnostic settings may change verbosity only within the audited metadata target allowlist; they cannot override the exclusion. Reject unsupported logging configuration or ignore the unsafe directives explicitly. Do not copy the echo server's unrestricted environment filter or its message logging. Redacted secret types must also be safe under Debug/panic/error formatting.

At the inspected rmcp 3.8 version, service debug events include complete requests and results. Re-audit this behavior on upgrades. Tests capture all configured sinks under maximum diagnostic settings and inject synthetic canaries for passwords, addresses, subject/body, search, folder, and provider-error text through success, failure, retry, and cancellation paths. No canary may appear. Do not rely on regex-redacting already formatted arbitrary messages.

## 14. Health and operation

/healthz reports process name/version, startup ID, read_only (true unless SMTP is enabled), and sending_enabled, without account data. It is process health, not proof of current mail authentication. A separate local connectivity check can validate TLS/authentication/capabilities; routine health polling does not repeatedly log in.

Support graceful termination and a documented supervised-service launch when deployment is implemented. Test locally before replacing the echo listener. Do not provision production credentials, change tailnet policy, replace running services, or perform mailbox actions merely to bootstrap the public repository.

## 15. Required verification and release gates

The implementation must include real HTTP MCP tests plus a controllable IMAP fixture. Capture actual wire commands/section requests, not only calls into a mocked domain adapter. No real credentials or mail belong in fixtures or public CI.

| Area | Required adversarial acceptance |
| --- | --- |
| R1 fetch limits | Tiny inline body plus large attachment: no attachment bytes fetched. Oversized declared literal, large headers, deep/wide MIME, huge SEARCH/LIST responses: fail before unlimited buffering; enforce measured byte/cache/worker budgets. |
| R2 logs | Maximum supported verbosity, all sinks, success/error/auth/timeout: no synthetic sensitive canary appears, including dependency events. |
| R3 identity | Initial and replacement enrollment have only intended permissions; known-listening forbidden service probes and policy assertions pass before real-mail deployment. An unresolved enrollment profile blocks deployment, not local coding. |
| R4 body cursor | Concatenated chunks equal the original decoded snapshot for base64, quoted-printable, multibyte charsets, Unicode, HTML, and JSON-escape-heavy content; change chunk size midstream and test expiry/deletion/restart. |
| R5 pages | Add arrivals, change flags, delete members, reduce output budget, replay cursors, and change query arguments: fixed descending UID membership, live flags, explicit errors, and no budget-skipped members. |
| R6 worker | Stall mid-literal, enqueue/cancel/expire jobs, disconnect callers: expired queued jobs issue no command; uncertain sessions are discarded; later work uses a clean session; queue/cache/decode reservations release. Failed credentials do not trigger a per-request login storm. |
| Read-only | Assert every emitted command is allowed and every body fetch uses PEEK. Refuse unexpected selection status. Teardown/reconnect/panic paths never mutate or expunge. |
| Compatibility | Unicode mailbox names, capability/charset failures, malformed headers/MIME, absent counts, protocol negotiation, schemas, and error envelopes. |

After automated gates pass, use designated synthetic messages in a live iCloud account: authenticate, discover capabilities/folders, list/search/read an unread message, confirm content/metadata and unchanged flags in Apple Mail, and repeat through the assistant on the approved endpoint. Exercise reconnect and deliberate credential revocation/reload. No live write/send test is part of V1.

V1 is complete when all four tools work against iCloud through the assistant, all R1–R6 requirements are implemented/tested, network enrollment gates pass, and the launch/credential procedures are documented. An echo success or a healthy HTTP process alone does not satisfy these gates.

## 16. Delivery and later phases

1. Bootstrap the separate public repository with this spec and review resolutions.
2. Pin compatible dependencies after checking bounded framing/decoding support; implement typed schemas and synthetic fixtures.
3. Implement credential/session handling, the four read tools, snapshots, budgets, worker state machine, and mandatory logging filters.
4. Pass automated gates, choose/validate restricted enrollment, and run controlled live acceptance.
5. Deploy the mail service on the approved endpoint with restart support.

Optional SMTP submission is implemented as a separate worker and contract in [sending.md](sending.md): fixed local sender/account, mandatory STARTTLS, durable idempotency and explicit uncertain outcomes. Reply threading, attachments and verified Sent-folder behavior remain later work. Mail organization still requires native UID MOVE/capability handling, destination references, partial results, and recorded/reconciled mutation outcomes. No IMAP mutation is implemented.

References: [IMAP4rev1](https://www.rfc-editor.org/rfc/rfc3501.html), [IMAP4rev2](https://www.rfc-editor.org/rfc/rfc9051.html), [IMAP MOVE](https://www.rfc-editor.org/rfc/rfc6851.html), [UIDPLUS](https://www.rfc-editor.org/rfc/rfc4315.html), [Tailscale tags](https://tailscale.com/docs/features/tags), [Apple settings](https://support.apple.com/en-us/102525).
