# Agent instructions

Read `docs/spec.md` and `docs/review-resolutions.md` before implementation.

- V1 is read-only. No mail mutation, sending, generic raw IMAP tools, or attachment-body retrieval.
- Preserve the normative limits, cursor semantics, worker cancellation, and logging exclusions. Add acceptance tests for implemented behavior.
- This repository is public. Use only synthetic messages, credentials, addresses, and network examples in committed files and logs. Keep private deployment configuration outside the checkout.
- Do not operate a live mailbox or change running services/policies as a side effect of building or testing the scaffold.
- If shared ai-core memory is available, consult the iCloud Mail MCP project under Clarkezone. Repository source/spec and current user instructions take precedence. Do not store secrets or mail data in memory.
- Verify changes with appropriate Rust formatting, Clippy, and meaningful tests. Scaffold checks alone do not establish mail-service correctness.
