# Optional SMTP submission — specification and operation

Revision 1, 2026-11-06 UTC. This is the explicitly requested sending phase. The four IMAP tools retain their read-only boundary. Sending is disabled by default and never enabled through MCP.

## Scope and configuration

Expose `send_message` and `get_send_status` only when `serve --sending-config PATH` loads a valid private configuration and an initialized journal. Default discovery still exposes four read tools. Sending-enabled discovery exposes six tools; health reports `read_only=false` and `sending_enabled=true`, without sender/account details or upstream calls.

This phase sends new UTF-8 plain-text messages with To/Cc/Bcc recipients. Attachment sending, HTML composition, reply/reply-all threading, sender selection by MCP, bulk campaigns, mailbox flag changes and IMAP APPEND are excluded. Bcc addresses enter the SMTP envelope and are stripped from message headers. The SMTP envelope deduplicates identical recipient addresses.

`configure-sending --config PATH` obtains the fixed From address and full SMTP login through hidden prompts. A blank SMTP login uses the From address. An alias may be configured locally with the account's real SMTP login; the provider still validates whether that From identity is authorized. SMTP uses the existing locally stored app-specific password. No passwords or sender addresses are MCP configuration arguments. Credentials and configuration are stored outside Git in a private directory (0700) and files (0600), with ownership/no-follow checks and atomic replacement. Existing Unix privacy hardening remains the supported model.

The configuration command explicitly initializes `send-journal.sqlite3` beside the configuration file. `serve` never silently creates a missing journal, initializes an empty database, recreates missing tables/keys, or repairs unsupported schema versions. Missing/corrupt/insecure state refuses sending startup. Stop the service before reconfiguring; an exclusive database lock prevents two active send workers using the same journal.

Enabling SMTP grants the principals admitted to this private MCP endpoint the ability to submit mail. Tailscale remains the application access boundary. Human approval/send-intent policy belongs to the calling assistant; tool annotations are metadata, not a server-side approval mechanism. Do not enable real-mail access through a broad or unverified enrollment identity. The existing restricted identity/lifecycle/network release gates still apply.

## Tool contracts

All arguments are typed; unknown fields are rejected. Header/address controls, display-name/group syntax and raw SMTP/header fields are rejected. Recipients are simple ASCII email addresses; internationalized domains must use their ASCII form. The body and subject may contain Unicode.

`send_message` requires:

- `idempotency_key`: canonical lowercase RFC-variant UUIDv4. The caller creates it once for one intended message and retains it across retries, timeouts, reconnects and restarts.
- `to`, `cc`, `bcc`: optional arrays of simple addresses; at least one and at most 50 total recipients; each address at most 254 bytes.
- `subject`: required string, at most 998 UTF-8 bytes, no control characters. Empty is permitted.
- `body_text`: required plain-text string, at most 32768 UTF-8 bytes, no NUL. Empty is permitted.

There is no From, arbitrary header, SMTP host/port, local file, attachment, alias-selection or journal-management input. The request remains subject to the existing 64 KiB serialized HTTP request cap, including JSON escaping. The encoded RFC message is at most 128 KiB and uses base64 text transfer encoding, bounded header construction, a stable Message-ID for the recorded attempt and a current Date. The MIME builder never uses the machine hostname for identifiers; SMTP EHLO uses a fixed generic name.

`get_send_status` takes only `idempotency_key`. It reads the local durable journal without SMTP or IMAP I/O.

Both return:

```text
idempotency_key, message_id, outcome, reason, smtp_code, updated_at
```

`outcome` is `accepted`, `not_accepted`, or `outcome_unknown`. `reason` is null for acceptance or a fixed code: `in_progress`, `interrupted`, `smtp_rejected`, `authentication_failed`, `tls_required`, `tls_error`, `connection_failed`, `connection_lost`, `protocol_error`, `deadline_exceeded`, `cancelled`, `journal_unavailable`. `smtp_code` is a numeric reply code when known; raw provider text is never returned or logged.

`accepted` means the SMTP server returned its final positive DATA acceptance. It does not prove recipient delivery or that a Sent-folder copy exists. The service does not append its own Sent copy, which avoids an unverified duplicate. Provider Sent-folder behavior and eventual delivery require separately authorized live acceptance tests.

For `send_message`, non-accepted outcomes set MCP `isError=true` while retaining the structured status. For `get_send_status`, an existing status is a successful query even when the stored outcome is negative/unknown. Missing keys, conflicts, unavailable/full journals, disabled sending and invalid messages use fixed error envelopes. Added error codes are `sending_disabled`, `idempotency_conflict`, `submission_not_found`, `journal_unavailable`, `journal_full`, `invalid_message`. Send annotations advertise readOnlyHint=false, destructiveHint=true, idempotentHint=false and openWorldHint=true. Status lookup is read-only/idempotent.

## Duplicate prevention and uncertain outcomes

Before any SMTP attempt, durably claim the UUID and bind it to an HMAC fingerprint of the complete typed request plus the configured SMTP account/From scope. Store an initial unknown/in-progress outcome before network I/O. Repeating the same key and payload returns its recorded result without another attempt. Changed payload or account scope under an existing key returns idempotency_conflict. A status key from another configured account/From scope is not exposed.

The private SQLite journal stores UUIDs, Message-IDs, keyed fingerprints, account-scope fingerprints, timestamps and fixed outcomes/codes. It stores no recipients, subjects, bodies, MIME payloads, SMTP login or password. Message-IDs can include the sender's domain and remain private metadata. The journal uses FULL synchronous commits, an exclusive lifetime lock, no-follow/ownership/mode checks, a 16 MiB database limit and a 10000-record cap. No automatic expiry/pruning can turn an old key into a new send. Existing records remain readable at capacity; new submissions fail with journal_full. SQLite rollback sidecars are protected by the private parent directory.

Restart converts unfinished in-progress records to unknown/interrupted. It never resends them. A network break/deadline before body transfer is known not accepted. Once body transfer may have completed, missing final acknowledgement is unknown unless an explicit negative SMTP completion was observed. A durable-result write failure also reports unknown. Polling status may resolve an attempt still running; a terminal unknown outcome requires manual reconciliation using Message-ID/provider evidence. A new key must never be generated automatically to recover an unknown outcome. After a confirmed not-accepted result, a deliberately authorized new attempt uses a new key; repeating the old key only retrieves the old failure.

This provides at most one SMTP attempt per retained key, not exactly-once downstream delivery. Keep the journal across restarts, credential changes and updates. Deleting/replacing the journal destroys duplicate-prevention history; any deliberate reset must first reconcile outstanding requests and prevent clients replaying old intents.

## Transport, worker and cancellation

Production connects only to smtp.mail.me.com:587. Require advertised STARTTLS and positive upgrade response, certificate-chain and hostname validation, and refreshed EHLO extensions after upgrading. Credentials are sent only after TLS. AUTH PLAIN supports an initial response and a single continuation. There is no plaintext, opportunistic TLS, invalid-certificate, alternate-host or generic SMTP command fallback. Plaintext fixture connectors compile only for tests.

SMTP lines are bounded at 4096 bytes before buffering; one multiline reply is at most 64 KiB/128 lines; cumulative SMTP response bytes are at most 128 KiB per attempt. Malformed/oversized replies close the connection. MIME construction/transfer is bounded independently. No raw provider response is diagnostic data.

A separate serialized send worker runs on one dedicated thread so SQLite/fsync cannot block the HTTP/IMAP Tokio workers. It admits at most 16 queued requests and one active attempt. It never uses the read worker's reconnect/retry loop, never pools/replays SMTP sends, and stops before DATA if any recipient is rejected. A negative final DATA result is recorded as not accepted; a final 250 records acceptance. Connection teardown does not wait for QUIT after a known outcome.

Queued canceled/expired/disconnected jobs issue no SMTP command. After a claim but before an attempt, cancellation records not accepted. Once an attempt begins, caller cancellation cannot undo submission: drive the attempt to its bounded outcome and record it, even if the caller disconnects. The attempt is bounded by the original admission deadline and at most 20 seconds. Global shutdown interrupts the connection, classifies the phase conservatively and preserves the preclaimed unknown record if the process exits before final persistence. The normal HTTP admission deadline remains 30 seconds.

A rejected SMTP authentication opens the send worker's process-wide breaker until deliberate restart. It does not prevent status lookup or replay of recorded results. It is separate from the read worker's authentication breaker. Health never authenticates.

## Verification and live acceptance

Synthetic tests exercise actual SMTP commands/DATA and real HTTP MCP, including opt-in discovery, accepted deduplication/concurrent calls, conflicts, restart/unfinished recovery, Bcc stripping, Unicode MIME, rejected recipients/final DATA, lost acknowledgements, pre/post-DATA deadlines, caller disconnect, canceled queued work, auth suppression, missing/failed TLS without AUTH, oversized replies, journal locks/permissions/capacity/scoping, missing/truncated startup state, and payload-free logging. No public fixture uses real account data.

Live SMTP sender authorization, recipient delivery and Sent-copy behavior remain unverified until an explicit test recipient and message are authorized. Building, schemas and synthetic tests neither contact iCloud nor send live email.

Sources: [Apple mail settings](https://support.apple.com/en-us/102525), [SMTP](https://www.rfc-editor.org/rfc/rfc5321), [STARTTLS](https://www.rfc-editor.org/rfc/rfc3207), [SMTP AUTH](https://www.rfc-editor.org/rfc/rfc4954), [SASL PLAIN](https://www.rfc-editor.org/rfc/rfc4616).
