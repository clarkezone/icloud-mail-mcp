# Agent instructions

Read `docs/spec.md`, `docs/review-resolutions.md` and `docs/sending.md` before implementation.

- The IMAP read path remains read-only. Optional SMTP submission is authorized only through the explicit sending configuration and docs/sending.md contract. No generic raw IMAP/SMTP tools, attachment retrieval/sending, or mailbox mutation.
- Preserve durable send idempotency, phase-aware uncertain outcomes, fixed sender identity, STARTTLS requirements and private journal handling. Never automatically retry a send with a new key or silently recreate missing sending state.
- Preserve the normative limits, cursor semantics, worker cancellation, and logging exclusions. Add acceptance tests for implemented behavior.
- Do not create public pull requests until GitHub web-commit email privacy has been verified: generated test-merge commits can use the account email. Use branch-only publication and local noreply merges meanwhile. Audit generated GitHub merge objects while private before ever making such a repository public.
- Use the repository-local GitHub noreply identity for commits. Audit author/committer metadata and source for account details before pushing; never restore pre-cleanup history.
- This repository is public. Use only synthetic messages, credentials, addresses, and network examples in committed files and logs. Keep private deployment configuration outside the checkout.
- Do not operate a live mailbox or change running services/policies as a side effect of building or testing the scaffold.
- If shared ai-core memory is available, consult the iCloud Mail MCP project under Clarkezone. Repository source/spec and current user instructions take precedence. Do not store secrets or mail data in memory.
- Verify changes with appropriate Rust formatting, Clippy, and meaningful tests. Scaffold checks alone do not establish mail-service correctness.
