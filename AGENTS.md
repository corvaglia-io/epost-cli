# Contributor instructions

This is a public repository. Treat all committed files, PR descriptions, CI output and task artifacts as public.

## Scope

Implement a Rust CLI and reusable library for inbox and archive retrieval only. Supporting authentication and explicit private/business tenant selection are in scope. Mail operations must remain read-only. Do not add sending, accept/reject, mark-read, archive/move, upload, restore or delete operations.

Do not confuse archive retrieval with moving a letter into the archive.

The maintainer also authorized an opt-in Paperless-ngx delivery destination.
Uploading verified local PDFs and creating the explicitly configured delivery
tags in Paperless are in scope. Keep ePost operations read-only. Paperless API
tokens are supplied only at runtime or saved with explicit opt-in in the local
macOS Keychain; never place them in config, reports, fixtures or CI. Develop and
test this integration exclusively against synthetic localhost servers.

## Data and credentials

- Use only public provider documentation and wholly synthetic test data.
- Never access, copy, record or publish personal or company credentials, tokens, mail documents, mail metadata, account identifiers, previous PoC downloads or production recordings.
- Do not request credentials to implement, test or review this project.
- Default tests and CI must be localhost-only and require no ePost secrets. Do not load a developer's environment credentials implicitly.
- Provider integration tests are separate, disabled by default and restricted to a dedicated sandbox account with synthetic data after access is confirmed. Never fall back to production.
- Do not print secrets or authenticated response bodies in errors or diagnostics. Test redaction using fake secrets.
- Ignore runtime configuration, state, downloads and secret files. Commit synthetic fixtures only in an explicitly named fixtures directory.

## Implementation and verification

- Read docs/cloud-task.md and docs/api-brief.txt before implementation.
- Delegate independent implementation/test review to subagents when available, with clear file ownership. Do not use multiple independent workers on the same files.
- Keep endpoint selection and redirects controlled so authentication cannot leak to an arbitrary host.
- Use bounded requests, retries and downloads. Respect Retry-After where applicable.
- Treat IDs, filenames, paths, URLs and API payloads as untrusted data.
- Download atomically, preserve existing good files and verify content before recording success. Report partial or unsupported retrieval explicitly.
- Tests must cover meaningful behavior, including refresh, pagination, partial failures and safe repeat runs.
- Run cargo fmt --check, cargo clippy --all-targets -- -D warnings and cargo test with a lockfile. Document any checks that could not run.
- Keep claims precise: mock tests do not establish provider compatibility or complete mailbox coverage.
- Do not redistribute the entire provider OpenAPI specification without checking its licensing. Reference its source and maintain only the minimal required contract information.

## Delivery

Work on a feature branch and produce a reviewable draft PR. Do not merge, force-push or change repository access/settings. Preserve existing changes. The maintainer selected the MIT license; preserve LICENSE and the package license declaration.

Versioned release CI is authorized: build and test public CI artifacts, and let matching version tags on commits merged into main publish GitHub releases after validation. Keep build/test permissions read-only and grant release writes only to the tag-triggered publisher. Do not create/push the first release tag or publish a release interactively without a separate explicit request. Crates.io publishing remains outside scope.
