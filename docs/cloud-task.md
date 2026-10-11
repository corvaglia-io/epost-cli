# Build the first Rust inbox/archive retrieval client

Implement the agreed first version in this repository. Use subagents for an independent test/security review if the cloud environment supports them. Keep edits coordinated and return a draft PR; do not merge it.

## Boundaries

This is a public, unofficial, best-effort client. Use public ePost documentation and synthetic data only. No user's personal or company ePost credentials, mail documents, real metadata or previous PoC artifacts may be used. Do not ask for credentials. Tests and development must not make authenticated provider calls or contact production mail APIs.

Only supporting authentication, inbox retrieval and archive retrieval are in scope. Sending and mailbox mutations are parked. In particular, do not accept/reject registered letters, mark them read, move/archive them, upload or delete anything.

## Required behavior

1. A reusable Rust library and the epost-cli binary, with a small command surface suitable for scripts and agents. Suggested commands: tenants, inbox list, inbox download, archive folders, archive list, archive download and a retrieval/sync command that makes its inbox/archive scope explicit.
2. Explicit tenant identity and mailbox kind. Tenant discovery must not silently select the first tenant or guess private versus business from a name or undocumented company ID convention. Support documented bearer/password authentication and refresh. Business API-key access may be supported if confirmed by the relevant public contract.
3. Document the user-managed private onboarding prerequisite: the ePost/KLARA password used for API authentication is distinct from SwissID authentication. Do not automate password setup or browser login.
4. Correct pagination over the relevant documented letter types and archive root/folders. Handle duplicate entries, unstable inventories and folders that cannot be enumerated through documented selectors. Do not report complete coverage when retrieval is incomplete or unsupported.
5. Download document content conservatively. Do not infer PDF or base64 solely from a schema label. Preserve unsupported structured/binary content with a clear status or fail safely, instead of producing a misleading PDF. Registered letters requiring acceptance remain pending and must be reported.
6. Safe output paths, atomic writes, hashes and a local manifest that supports repeat runs, distinguishes inbox/archive sources and detects existing corrupt files. Avoid file overwrites and cross-tenant collisions. Handle crashes, bounded concurrency and conflicting concurrent runs.
7. Bounded timeouts and retries, token refresh with rotated refresh tokens, rate-limit handling, no uncontrolled redirects and no credential leakage across hosts. Fail with useful redacted diagnostics.
8. Human-readable help plus documented structured JSON output. Define stable nonzero exit codes for authentication failure and partial retrieval so an automation engine can alert instead of recording silent success.
9. Runtime credential input must avoid command-line password arguments and logging secrets. No default persistent token/password storage. Document how users can supply runtime credentials separately from project tests.

## Tests and public CI

Use a local HTTP mock server with synthetic tenants, tokens, letter metadata and generated documents. Validate request forms, query parameters and response parsing against the relevant OpenAPI contracts. Add explicit behavior scenarios; generated schema mocks alone are insufficient.

Cover private/business request selection; token expiry, refresh and rotation; 401/403; rate limits and bounded retries; inbox and archive pagination; duplicate letters; archive root/subfolders and unsupported selectors; awaiting-acceptance letters; PDF and JSON/binary content; truncated downloads; interrupted/repeated runs; path traversal and cross-tenant collisions; manifest corruption; concurrent runs; secret redaction; and partial-result exit codes.

The default suite must work with no provider secrets and may contact localhost only. Provider integration tests, if included, are separate and disabled by default; allow only a confirmed sandbox host and dedicated test credentials. A production URL must be rejected by this test suite, and missing sandbox configuration must never trigger a production fallback. Sandbox access and private-mailbox parity are unverified because official documentation conflicts.

Add minimal GitHub Actions CI with read-only permissions and formatting, clippy and tests. Pin action dependencies appropriately. Commit Cargo.lock for the CLI. Declare a realistic Rust toolchain/MSRV based on what the environment actually supports. The maintainer subsequently authorized versioned release CI; see docs/releases.md for its scope and activation.

## Documentation and handover

Read docs/api-brief.txt for exact API shape notes and authoritative links. Update README to describe installation, examples, configuration, return values, credential separation, scheduler-friendly use, known limitations and what was actually tested. Examples must use invented values only. Explain that downloading mail does not perform any required manual acceptance or subsequent action on its contents.

Provide a short implementation summary, test results, limitations and unresolved provider questions. Clearly identify mock-tested behavior and leave provider compatibility unverified until separately tested. The maintainer selected MIT during implementation. Do not publish a crate or binary release without the maintainer's direction.
