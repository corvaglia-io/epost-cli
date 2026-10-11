# epost-cli

An unofficial Rust CLI and library for retrieving documents from a private or business ePost inbox and archive. It makes deterministic API requests; no browser or LLM is needed at runtime.

This first version is tested against synthetic localhost servers. **Provider compatibility and complete mailbox coverage remain unverified.** The project is independent of ePost, KLARA and Swiss Post and offers no guarantee of availability or correctness.

## Scope

- Explicit private/business mailbox selection and tenant discovery.
- Password/bearer authentication, in-session token refresh and business API keys.
- Inbox metadata, archive folders/root/folder metadata, and document retrieval by letter ID.
- Bounded pagination and retries, atomic local storage, hashes and repeat-run verification.
- JSON reports and nonzero exit codes suitable for scripts, agents, n8n or Dagu.

Mail operations are read-only. The client does not accept/reject registered letters, mark letters read, move/archive, send, upload, restore or delete mail. Authentication uses the documented POST endpoints. Downloading a document does not perform required manual acceptance or act on its contents.

## Build and install

Install Rust using [rustup](https://rustup.rs/), then build from this repository:

```sh
git clone https://github.com/corvaglia-io/epost-cli.git
cd epost-cli
cargo build --release --locked
./target/release/epost-cli --help
# Optional installation into your Cargo binary directory:
cargo install --path . --locked
```

The toolchain and declared minimum Rust version are 1.98.0. Development is verified on macOS; CI is configured to check Linux. No crate, binary release or container image is published. The project uses the [MIT license](LICENSE).

## Authentication and mailbox selection

For private accounts, follow the provider's [private tenant onboarding guide](https://developer.epost.ch/docs/api-docs/v6nqmmjkxcery-how-to-access-the-letterbox-public-ap-is-with-a-private-tenant). API password authentication uses the account email and a separate **ePost/KLARA password**, distinct from the SwissID password. Onboarding and password setup remain user-managed.

Run tenant discovery interactively:

```sh
epost-cli --json tenants
```

Prompts stay off stdout: the email prompt uses stderr, and the hidden password prompt uses the terminal. Discovery never chooses a tenant. Every subsequent command requires all three values: `--tenant-kind private|business`, `--tenant-id`, and `--company-id`. Supply the IDs returned by discovery and select the kind yourself; the client does not infer it from company IDs or names. Password login checks that the supplied tenant/company pair appears in discovery.

Runtime credentials can instead be supplied by your own secret manager:

| Input | Behavior |
| --- | --- |
| `EPOST_USERNAME` and `EPOST_PASSWORD` | Password login and in-session refresh |
| `EPOST_USERNAME` with `--password-stdin` | Read one password line from stdin; replaces password environment/prompt input |
| `EPOST_BEARER_TOKEN` | Existing bearer token; no refresh token is available in this mode |
| `EPOST_API_KEY` | Business mailbox only; does not support tenant discovery |

Bearer token and API key are mutually exclusive; neither may be combined with `--password-stdin`. They take precedence over password environment variables. There is no command-line password argument, automatic `.env` loading, keyring access, persistent session or stored password/token. Each invocation authenticates anew. Credentials stay in process memory; the program cannot guarantee erasure of every copy made by the OS or HTTP libraries.

For unattended use, supply credentials explicitly through the scheduler's secret mechanism or a password pipe. Keep runtime secrets outside the repository and public CI. Reports and downloaded documents contain private information; choose a private output location and keep reports out of public logs.

## Commands

The following IDs are invented examples. Replace them with your explicitly selected mailbox values. If no credential environment variables are set, these commands prompt interactively.

```sh
epost-cli --json --tenant-kind private --tenant-id example-tenant --company-id 0 inbox list --all
epost-cli --json --tenant-kind private --tenant-id example-tenant --company-id 0 archive folders
epost-cli --json --tenant-kind private --tenant-id example-tenant --company-id 0 archive list --root --all
epost-cli --json --tenant-kind private --tenant-id example-tenant --company-id 0 archive list --directory-id example-folder --all
epost-cli --json --tenant-kind private --tenant-id example-tenant --company-id 0 show example-letter
epost-cli --json --tenant-kind private --tenant-id example-tenant --company-id 0 download example-letter --output ./data
epost-cli --json --tenant-kind private --tenant-id example-tenant --company-id 0 sync --scope both --output ./data
```

`download` works for inbox or archived letter IDs. `sync --scope inbox|archive|both` makes the retrieval scope explicit and stores documents across that scope. Archive sync enumerates the flat folder list, visits root storage, then each unique selectable directory without assuming a single parent or recursively walking folder links. `archive folders` preserves the flat directory metadata, including multiple-parent IDs.

`list` fetches one page by default (`--page-size 48 --offset 0`). Use `--all` to traverse from offset zero; `--offset` and `--all` cannot be combined. Traversal stops after a short/empty page or an error/safety bound. Limits are page size 1–1000, default 1000 pages, at most 100,000 entries per inventory, and at most 100,000 retrieval records per sync. Detected overlap, missing IDs, repeated pages and unsupported directory selectors produce a partial result. A single-page command can succeed without claiming pagination completeness.

## Output and exit codes

Default output is pretty-printed JSON; `--json` emits compact JSON. Help and version are ordinary text. Successful and partial commands write one report to stdout:

```json
{"schema_version":1,"status":"ok","data":{"source":{"type":"inbox"},"letters":[],"pagination_complete":true,"pages":1,"issues":[]}}
```

Fatal errors write a redacted diagnostic to stderr. With `--json`:

```json
{"schema_version":1,"status":"error","exit_code":3,"error":"authentication failed"}
```

| Exit | Meaning |
| --- | --- |
| 0 | Requested operation completed; this is not proof of full mailbox coverage |
| 1 | Configuration, transport, response or local I/O failure |
| 2 | Partial/unsupported retrieval, unfinished inventory, or invalid CLI usage |
| 3 | Authentication failed (including HTTP 401/403) |
| 4 | Storage integrity failure or conflicting store access |

For reports with multiple failures, storage integrity takes precedence over authentication, then partial retrieval. Authentication loss stops further requests. Later-page errors retain previously listed metadata; sync can retrieve those letters after non-authentication page failures. Fatal errors have no normal stdout report. Callers should ignore unknown JSON fields and check both exit status and individual retrieval records.

Sync reports include inventory counts/issues, `pagination_complete`, individual document statuses, and `mailbox_coverage_verified: false`. Document statuses include `saved_pdf`, `saved_json`, `saved_binary`, `already_saved`, `awaiting_manual_acceptance_or_status_verification`, `failed`, and `deferred_due_to_authentication_failure`. JSON/binary content is preserved but counts as unsupported retrieval (exit 2), including on repeat runs. Pending/unknown registered-letter states remain partial even if previously cached content exists.

Save the report somewhere private and configure your automation engine to alert on **every nonzero exit**, including exit 2. A successful download alone is insufficient evidence that all mail has been noticed or acted on. Scheduling, notifications and Paperless ingestion are outside this version.

## Storage and request limits

`--output` names a local document store, not a particular filename. A SHA-256 namespace derived from API origin, tenant/company IDs and mailbox kind separates mailboxes. Hashed letter IDs determine document paths; provider filenames are never used as paths. A private manifest records content hashes, byte counts, media classification and inbox/archive sources. On Unix, managed directories use mode 0700 and files 0600. stdout report redirection permissions remain the caller's responsibility.

The store holds an exclusive namespace lock, validates recorded files before skipping them, and refuses conflicting or corrupt content. Document writes use temporary files and atomic finalization; interrupted manifest writes can recover only matching complete content. Symlink paths and managed hardlinks are rejected. Use a physical output path without symlink ancestors or `..` components; this also applies to macOS aliases such as `/tmp` (use `/private/tmp` when relevant). These checks do not make the store a security boundary against another malicious process running as the same user. Windows does not provide the Unix permission/no-follow guarantees and is unverified.

Responses are buffered in bounded memory before being written atomically. The default response cap is 32 MiB, configurable with `--max-response-mib 1..64`. A recognized PDF must have a PDF header and terminal EOF marker; this is a truncation check, **not full PDF validation**. Valid JSON is stored unchanged as `.json`; other nonempty bytes as `.bin`. Detected or declared PDF/JSON must pass these checks; malformed or truncated content fails instead of being relabeled as binary. No base64 conversion or arbitrary content-reference URL fetching occurs.

Default request timeout is 30 seconds (`--timeout-seconds 1..120`). GET responses with HTTP 429/502/503/504 are retried up to twice (`--retries 0..5`); Retry-After is honored within a 30-second wait bound, with larger delays failing instead of retrying early. Network failures are not automatically retried. Authentication POST requests are not retried. Token expiry triggers refresh; an HTTP 401 permits at most one refresh/replay. Redirects and environment proxy discovery are disabled. There is no arbitrary production base URL or insecure TLS option.

## Tests and CI

```sh
cargo fetch --locked
cargo fmt --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo test --locked --offline
```

All 76 integration scenarios use invented identifiers, fake secrets, generated bytes and literal loopback HTTP servers. Spawned CLI processes clear inherited environment variables and never load a developer's credentials. Tests cover request forms/query encoding, private/business selection, token expiry/rotation, status handling, pagination/partial reports, registered-letter deferral, JSON/binary preservation, interrupted writes, safe repeats, corruption, path attacks, concurrent store access and secret redaction. Unix storage tests are conditionally compiled. CI uses a Linux network namespace with only loopback networking available during test execution; dependency fetching happens earlier.

No provider integration suite or authenticated provider test is included. `--environment sandbox` selects only the documented test host for deliberate runtime use; it never falls back to production. Sandbox access and private-mailbox parity remain unresolved because official documentation conflicts.

## Known provider limitations and references

Offset lists expose no snapshot, total or cursor; even normal termination cannot establish a consistent complete mailbox inventory. Inbox requests include all six documented letter types; future types may need an update. Branded archive directories can have empty IDs with no documented distinct selector, so sync reports partial rather than treating them as root storage. The content schema is ambiguous about PDF versus JSON/binary responses. Registered-letter acceptance remains a manual action.

Contract research was performed on 2026-10-11. Only the required contract notes are maintained in [docs/api-brief.txt](docs/api-brief.txt); the full provider specification is linked, not redistributed.

- [Production Swagger](https://api.epost.ch/docs) and [OpenAPI JSON](https://api.epost.ch/openapi?query=All)
- [Private tenant authentication](https://developer.epost.ch/docs/api-docs/v6nqmmjkxcery-how-to-access-the-letterbox-public-ap-is-with-a-private-tenant)
- [Inbox listing](https://developer.epost.ch/docs/api-docs/f52fd27fd878a-get-letters-of-current-user), [content](https://developer.epost.ch/docs/api-docs/f3d6c00359743-get-the-content-of-a-letter-with-given-id)
- [Archive folders](https://developer.epost.ch/docs/api-docs/048e72f82314c-get-all-folders-branded-folders-unbranded-folders-and-subfolders-in-the-storage), [archive listing](https://developer.epost.ch/docs/api-docs/c0ca4f1606215-get-letters-in-root-storage-or-in-a-specific-folder)
- [Test onboarding](https://developer.epost.ch/docs/api-docs/z6ydxfa37ns58-how-to-get-started-on-our-test-environment) and [TEST Swagger](https://api-test.klara-epost.tech/docs)

Contributor boundaries and the original implementation brief are in [AGENTS.md](AGENTS.md) and [docs/cloud-task.md](docs/cloud-task.md).
