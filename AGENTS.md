# Contributor instructions

This is a public, unofficial, MIT-licensed project. Treat commits, PR text, CI logs
and artifacts as public. Preserve LICENSE and the package license declaration.

## Scope

- Rust CLI/library for explicit private/business mailbox authentication and
  read-only inbox/archive retrieval. Do not add ePost sending, mark-read,
  accept/reject, move/archive, restore, upload or delete operations.
- Configured Paperless-ngx PDF uploads and verified folder copies are supported.
  All configured destinations participate by default. Preserve profiles,
  credential bindings and delivery history during migrations.
- Terminal UI, structured output, optional native macOS Keychain credential
  storage, destination tags/metadata and read-only diagnostics are supported.

## Privacy and testing

- Use public provider documentation and wholly synthetic test data only.
  Never import real credentials, account identifiers, mail, metadata, profiles,
  recordings or earlier download artifacts into development/tests/CI.
- Do not request credentials to implement or review the project. Keep runtime
  state outside the repository; never log secrets or authenticated response bodies.
- Default tests are localhost-only, clear inherited credential variables and use
  fake secret stores. Never access the host Keychain in tests. Any future provider
  integration suite must be separate, disabled by default and restricted to a
  confirmed sandbox with synthetic data; never fall back to production.

## Implementation

- Read `docs/api-brief.txt` for the minimal public contract. Reference the full
  OpenAPI source; do not redistribute it without checking licensing.
- Delegate independent implementation/test review to subagents when available,
  with separate file ownership.
- Keep authenticated endpoints and redirects controlled. Bound requests, retries,
  pagination and downloads; honor Retry-After. Treat provider data as untrusted.
- Use verified atomic writes and safe paths; preserve good files. Report partial
  inventories, unsupported content and uncertain delivery explicitly.
- Credential persistence requires opt-in; use the local nonsynchronized macOS
  Keychain. Never store passwords/tokens in profiles, reports or fixtures.
- Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
  `cargo test --locked` and relevant release-script checks. Keep Cargo.lock.
  Mock tests do not establish provider compatibility or complete coverage.

## Delivery

Work on a feature branch and provide a reviewable draft PR. Do not merge, rewrite
remote history or change repository settings without an explicit user request.
Release CI publishes matching version tags on main after validation. Keep tests
and builds read-only; grant release writes only to the tag-triggered publisher.
Do not publish tags/releases or crates without explicit direction.
