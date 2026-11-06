# Adversarial review resolutions

Revision 0.2, 2026-11-05 UTC. This records design resolutions and implementation status. The earlier review in the project memory remains the historical record. Automated evidence is described below; live acceptance remains pending.

| Finding | Resolution in spec.md | Acceptance evidence required |
| --- | --- | --- |
| R1: output caps did not bound fetch/parse work | Sections 8/10 mandate section-only fetching, pre-allocation framing guards, explicit wire/decode/MIME/cache limits, and no attachment-body fetches. | Wire traces and bounded-resource synthetic fixtures, including malicious literal lengths and large attachments. |
| R2: SDK logging could leak mail | Section 13 mandates metadata-only targets and immutable dependency exclusions that diagnostic settings cannot override; provider errors are sanitized before formatting. | Canaries absent from every sink at maximum supported verbosity through success/failure/cancellation. |
| R3: manual tagging did not survive identity replacement | Section 3 requires restricted enrollment from the first connection, lifecycle tests, and known-listening negative probes. Personal-account enrollment with a temporary broad-access window fails the deployment gate. | Verify a supported tagged join or dedicated restricted account and test actual re-enrollment; integration capability remains to be established. |
| R4: decoded cursors were ambiguous | Section 9 chooses bounded immutable UTF-8 body snapshots with identity/section/representation binding, actual-byte progress, fixed expiry, and explicit stale/missing errors. | Concatenated output equality across encodings, HTML, JSON escaping, variable chunks, replay, and invalidation. |
| R5: search/list consistency was unspecified | Sections 6/7 choose bounded fixed membership, descending UID order, live values, bound query arguments, and continuation only over emitted/unavailable entries. | Arrivals, flag changes, deletion, replay, response-budget shrinkage, and mismatched query tests. |
| R6: queued cancellation could execute stale work | Sections 4/11 specify admission-time deadlines, queue capacity, cancellation propagation, immediate stream discard, bounded decoding slots, retry budgets/backoff, and a credential-generation auth breaker. | Mid-literal stalls and expired queued jobs produce no stale commands; reservations release and later work uses a clean session. |

## Implementation status

Specification revision 0.3 adds an explicit local iCloud EXAMINE compatibility option. Strict READ-ONLY enforcement remains the default; the option accepts only a tagged READ-WRITE selection marker and retains UID identity validation, the non-mutating command allowlist and BODY.PEEK reads. Its synthetic regression checks unread preservation, blocked writes and invalid mailbox identity.

The service implements the controls for R1, R2, R4, R5, and R6. All 34 automated tests pass, including synthetic IMAP wire fixtures and actual HTTP MCP calls. They exercise framing/decoder bounds, attachment non-retrieval, payload logging exclusions, immutable UTF-8 chunks, fixed membership with live flags, authentication suppression, queue limits, and cancellation. See [implementation notes](implementation.md) for module boundaries and coverage.

These checks do not establish every live acceptance case in the specification. R3 remains a deployment gate: the actual Grokbot enrollment mechanism, replacement lifecycle, and negative network probes have not been verified for this service. Live iCloud/provider behavior and Grokbot tool calls also remain pending.

Read-only scope, local upstream credentials, and Tailscale-based downstream access control are retained. Public fixtures and documentation use synthetic data. Account setup and provider checks run separately with explicit local authorization; they do not alter tailnet policy.

## Adversarial implementation review

- Fixed quadratic HTML conversion work for an unterminated tag. The converter consumes the remaining malformed tail once and continues to suppress script/head/style/template content. A bounded-time regression covers a long malformed tail both in visible content and a suppressed script.
- Fixed quoted-printable decoding accepting a leading plus sign as a hexadecimal digit. Transfer escapes now require two hexadecimal characters; malformed escapes produce unsupported_content. The regression reproduces the prior acceptance and checks valid escapes.
- Strict and explicitly enabled EXAMINE modes, immutable cursors, read-command restrictions, logging exclusions and synthetic HTTP integration remain covered. Deployment identity/network acceptance remains separate from this code review.

## Optional sending phase

The separately authorized sending contract is [sending.md](sending.md). Its synthetic suite covers real SMTP commands and HTTP discovery/calls, durable duplicate prevention and restart recovery, private Bcc headers, negative/unknown outcomes, deadline/cancellation phases, TLS downgrade rejection, bounded replies, journal protection and logging exclusions. Missing sending state is not silently recreated. No live delivery or Sent-folder acceptance is established by these checks.
