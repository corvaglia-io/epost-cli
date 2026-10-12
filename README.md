# epost-cli

An unofficial Rust CLI and library for retrieving documents from a private or business ePost inbox and archive. It makes deterministic API requests; no browser or LLM is needed at runtime.

This client is tested against synthetic localhost servers. **Provider compatibility and complete mailbox coverage remain unverified.** The project is independent of ePost, KLARA and Swiss Post and offers no guarantee of availability or correctness.

## Start here

```sh
epost                         # Ratatui menu and onboarding
epost destination add         # optional Paperless/folder destination
epost check inbox             # fetch new mail and deliver to all destinations
epost doctor                  # diagnose access and pending work
epost --non-interactive --json check inbox  # scheduler/agent use after setup
```

`epost-cli` remains an equivalent executable name. On terminals, commands show
live progress and a compact text summary; the menu keeps a result screen open
with next actions and `d` for details. Pipes use plain text, `--details` expands
technical text diagnostics, and `--json`
provides structured results. The saved default profile supplies account settings
and Keychain preferences. **Every configured destination is used by default**;
repeat `--destination NAME` to restrict one run, or use `--local` for local retrieval.
See the [command map and migration guide](docs/commands.md) for setup,
multiple accounts, verified folder copies, resuming delivery and Doctor.

## Scope

- Explicit private/business mailbox selection and tenant discovery.
- Guided setup, readable inbox/archive browsing and numbered document selection.
- Reusable mailbox/output/scope profiles with a remembered username and optional macOS Keychain password storage.
- Password/bearer authentication, in-session token refresh and business API keys.
- Inbox metadata, archive folders/root/folder metadata, and document retrieval by letter ID.
- Bounded pagination and retries, atomic local storage, hashes and repeat-run verification.
- JSON reports and nonzero exit codes suitable for scripts, agents, n8n or Dagu.
- Opt-in delivery of locally verified PDFs to Paperless-ngx, with an inbox review tag, resumable ingestion and original-byte verification.

ePost mail operations are read-only. The client does not accept/reject registered letters, mark letters read, move/archive, send, upload, restore or delete ePost mail. Authentication uses the documented POST endpoints. Paperless delivery is a separately configured destination; the new check routine uses all configured destinations, while legacy sync remains opt-in. Downloading or delivering a document does not perform required manual acceptance or act on its contents.

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

The toolchain and declared minimum Rust version are 1.98.0. Development is verified on macOS and Linux CI. The project uses the [MIT license](LICENSE).

## Versioned binaries

Release CI builds macOS and Linux archives for x86-64 and ARM64. A tag such as `v0.1.0` must match `Cargo.toml` and point to a commit merged into `main`. After checks and all four builds succeed, the workflow creates a GitHub release containing the archives and `SHA256SUMS`. PR/manual runs build preview artifacts without creating a release. Archives become available when a maintainer pushes a matching version tag after merging the release workflow; source builds remain available.

Choose your platform archive from [GitHub Releases](https://github.com/corvaglia-io/epost-cli/releases), verify its checksum, then extract and run the included binary. Each archive contains `epost`, `epost-cli`, `README.md` and `LICENSE`. Linux builds use glibc on Ubuntu 22.04 and are not static/Alpine binaries. macOS binaries are unsigned and not notarized. No Windows binary, crate or container image is published. See [release instructions](docs/releases.md) for platform names, verification and maintainer steps.

## Authentication and mailbox selection

For private accounts, follow the provider's [private tenant onboarding guide](https://developer.epost.ch/docs/api-docs/v6nqmmjkxcery-how-to-access-the-letterbox-public-ap-is-with-a-private-tenant). API password authentication uses the account email and a separate **ePost/KLARA password**, distinct from the SwissID password. Onboarding and password setup remain user-managed.

### Guided setup and browsing

Start in a terminal:

```sh
epost-cli setup --profile personal
epost-cli --profile personal browse
epost-cli --profile personal sync
```

Setup prompts for sign-in, mailbox kind and a numbered mailbox choice, even when only one mailbox is returned. Choose a private document folder (the default is `~/Downloads/epost-cli-data`) and a sync scope: inbox, archive or both. It remembers the account email, so later runs do not ask for the username again. On macOS, setup also offers to save the password in the local Keychain; this requires an affirmative choice. Without that choice, later runs still prompt for the password. Invalid choices retry. Type `q` at a choice prompt to cancel; EOF also ends the session. An existing profile requires confirmation before replacement; concurrent changes cause setup to stop rather than overwrite the newer profile.

`browse` signs in once for the session. It shows readable inbox and archive pages, downloads documents by number, and offers a sync action. Archive root is a separate choice; folders without usable API selectors are shown as unavailable. Registered letters that need acceptance or status verification are shown as requiring action in ePost. A displayed page is not a complete mailbox inventory. Use `epost-cli browse` without a profile for a temporary session with explicit mailbox selection.

Profiles save mailbox kind/IDs, environment, document folder, sync scope and, optionally, the account email and password-source preference. **The profile configures where to retrieve and save documents; it is not a persistent authenticated session.** Passwords and tokens never go into its JSON file. The default file is `~/Library/Application Support/epost-cli/config.json` on macOS, and `$XDG_CONFIG_HOME/epost-cli/config.json` (or `~/.config/epost-cli/config.json`) elsewhere. Use the global `--config /absolute/path/config.json` for a different file; include it in subsequent commands. Profile files contain private account identifiers, use private Unix permissions, and belong outside this repository. Setup and browsing require a terminal and password authentication; bearer/API-key users can use the ordinary script commands.

Existing profiles from earlier versions remain valid but have no remembered login. Update one without repeating mailbox and output setup:

```sh
# Verify sign-in and remember the username; keep prompting for the password:
epost-cli --profile personal login
# macOS only: verify sign-in and explicitly save the password in Keychain:
epost-cli --profile personal login --remember
# Forget the local Keychain password, retaining the username and mailbox settings:
epost-cli --profile personal logout
```

Keychain storage is local and not synchronized through iCloud. Entries are bound to the exact profile, account, environment, mailbox identity and config location. Copying a profile or changing those settings does not copy or reuse a saved password. Access needs an available, unlocked login Keychain; denied or missing access gives a redacted error with a recovery action. The macOS method is unsupported on other platforms. `logout` only forgets the local saved password and changes its source back to prompting; it does not revoke provider sessions or change mail.

On another OS, `login` without `--remember` can convert a copied Mac profile to username-only prompting after successful sign-in. It cannot delete the saved password on the original Mac; use `logout` there if required.

A selected profile supplies mailbox identity and environment. Explicit identity/environment flags cannot be combined with it. Download and sync may override the saved destination with `--output`; sync may override the saved scope with `--scope`. Without a profile, the existing explicit flags are required. Existing script commands retain their structured reports when `--json` is supplied; guided commands write their readable interface to stderr and reject `--json`/`--password-stdin`.

### Paperless delivery

For direct Paperless delivery, see [Paperless setup and recovery](docs/paperless.md). After configuring a destination with `epost-cli --profile personal paperless setup --url https://paperless.example.invalid --remember` (macOS), use `paperless upload` to deliver saved PDFs without an ePost login, or `sync --paperless` to retrieve and deliver. The token prompt is hidden; setup without `--remember` uses runtime token input instead. ePost archiving remains a [future improvement](docs/future-improvements.md).

### Inbox checks

```sh
# One unread-count request; no local store access or document downloads
epost-cli --profile personal inbox check --unread-only
# Also find inbox IDs not yet recorded in the local store
epost-cli --profile personal inbox check
# Sync the inbox only when unread or unsaved items are observed
epost-cli --profile personal inbox check --sync-if-needed
# Cheap unread-only trigger (does not catch already-read unsaved mail)
epost-cli --profile personal inbox check --unread-only --sync-if-needed
# Structured counts and trigger flags for a scheduler
epost-cli --json --profile personal inbox check
```

The API supplies `READ`/`UNREAD` metadata and a dedicated unread-count endpoint. Reading or downloading content does not mark it read. “New” here means an inbox ID absent from this profile's local document manifest; it is not the provider's read status or a first-seen timestamp. A saved document can remain unread and keep triggering a conditional check; repeated syncs verify and reuse its file. Unsaved failures remain eligible for another sync.

The default check walks bounded inbox metadata pages and opens/validates the local store without fetching content or changing its manifest. This includes local integrity verification, so `--unread-only` is cheaper for frequent polling. That mode makes one count request and does not evaluate unsaved mail; zero unread is not proof that all documents are saved. Conditional sync is explicitly opt-in, always targets the inbox (even if the profile's normal sync scope is both), and refreshes the metadata inventory before retrieving eligible content. Archive additions need a regular archive/both sync.

JSON checks expose `has_unread`, `has_new`, `sync_needed`, `sync_requested` and `sync_performed`. A missing/unknown read status or unfinished inventory produces exit 2; unknown answers are `null`, never a misleading false. Positive observations can still trigger the requested sync after non-authentication partial checks; authentication failure stops further requests. A sync attempt sets `sync_performed: true`. A completed retrieval report is nested under `sync` with `sync_exit_code`; a fatal attempt instead has a redacted `sync_error` and `sync_exit_code`. `check_exit_code` retains the original check result, and the final exit preserves any check or sync failure. Exit 0 means the check succeeded, regardless of whether it found mail; schedulers should inspect the flags. Without a profile, supply explicit mailbox identity and `--output` for the full check or conditional sync. A standalone `--unread-only` check needs no output folder.

[Official OpenAPI](https://api.epost.ch/openapi?query=All) documents the read filter and `GET /epost/v2/letters/inbox/count`; count and offset APIs provide observations without snapshot or complete-coverage guarantees.

### Repeat syncs and scheduling

```sh
# Use a saved destination/scope and supply credentials by prompt:
epost-cli --profile personal sync
# Force progress even when stderr is redirected, keeping the JSON report on stdout:
epost-cli --progress --json --profile personal sync
# Suppress progress:
epost-cli --no-progress --profile personal sync
# Scheduler/agent use: supply runtime credentials through its secret mechanism:
epost-cli --json --profile personal sync
```

Sync inventories the selected scope on each run. It verifies saved documents against the local manifest and downloads only unsaved content. A repeat reports `already_saved`; newly discovered documents are downloaded. This is not a provider change-feed or a date watermark: scanning all configured pages avoids intentionally skipping older documents that become visible later. Failed documents can be retried on the next run. Integrity failures require attention, and pending registered letters remain partial results. Keep the same store between runs and alert on every nonzero exit. Scheduling and Paperless ingestion remain external.

Sync shows sign-in, inventory pages, document checks and retrieval outcomes on stderr when stderr is a terminal and `--json` is absent. The final human summary counts saved documents, verified existing PDFs and items requiring attention. Other formats include both new and cached non-PDF content and are counted separately from reused PDFs. `--progress` enables progress explicitly, including alongside `--json`; `--no-progress` disables it. Progress displays counts and fixed outcomes without document titles or IDs. It is based on work observed so far and does not claim a mailbox total or complete coverage.

With stdout connected to a terminal, standalone sync shows a readable result summary by default. `--json` forces a structured report, so scripts can save the report while explicit progress remains visible on stderr. Redirected output without `--json` is plain text. For example, `epost-cli --progress --json --profile personal sync > /absolute/private-folder/report.json` writes detailed results outside this repository. The caller must choose a private destination and file permissions.

A partial result is not necessarily a failed download: pending registered letters need manual action or status verification in ePost; JSON/binary content is preserved but needs format review; unsupported archive selectors or interrupted pagination leave inventory incomplete. The report distinguishes these reasons. On a content HTTP 404, sync makes one bounded metadata check at the canonical letter endpoint. The result distinguishes available metadata with unavailable content, unavailable letter metadata, and an unsuccessful metadata check. A changed pending registered status still requires manual action. A 404 does not prove deletion, and message metadata is never substituted for a document or cached as a successful download. The JSON `diagnostic` contains fixed reason codes, HTTP statuses, a known letter type and presence flags; it contains no message body or reference URL. Repeating a run reuses verified saved files and retries eligible failures; it cannot resolve acceptance requirements or unsupported provider selectors.

When content returns 404 but matching metadata remains accessible, the error explicitly says that the **public API cannot download the item** and recommends checking or downloading it manually in the ePost app or website. JSON retains `diagnostic.reason_code = "content_unavailable_metadata_available"` and adds `diagnostic.recovery_action = "check_or_download_in_epost"` for this verified case. This does not diagnose deletion, onboarding mail, scanned versus digital origin, or website availability. Pending registered items retain their acceptance/status guidance. Retrieval still reports `failed` and exit 2; no negative result is cached as success. A download made separately in the app or browser is not automatically registered in the CLI store.

An authenticated website-download fallback is a [possible future improvement](docs/future-improvements.md#authenticated-website-download-fallback). It is not implemented, and the CLI does not probe website endpoints or collect browser sessions to diagnose a public API 404.

For a container or remote scheduler, provision the profile file and persistent document folder on that host and select the config path explicitly. Supply credentials through that scheduler's secret mechanism; a Mac Keychain entry does not travel with the profile. A local macOS scheduler can use an explicitly saved Keychain password if its login Keychain is accessible. Do not place private profiles or runtime credentials in public CI.

Run tenant discovery interactively:

```sh
epost-cli --json tenants
```

Prompts stay off stdout: the email prompt uses stderr, and the hidden password prompt uses the terminal. Discovery never chooses a tenant. Every subsequent command requires all three values: `--tenant-kind private|business`, `--tenant-id`, and `--company-id`. Supply the IDs returned by discovery and select the kind yourself; the client does not infer it from company IDs or names. Password login checks that the supplied tenant/company pair appears in discovery.

Runtime credentials can instead be supplied by your own secret manager:

| Input | Behavior |
| --- | --- |
| `EPOST_PASSWORD` with `EPOST_USERNAME` or a remembered profile username | Password login and in-session refresh; overrides the saved password source |
| `--password-stdin` with `EPOST_USERNAME` or a remembered profile username | Read one password line from stdin; replaces password environment, Keychain and prompt input |
| `EPOST_BEARER_TOKEN` | Existing bearer token; no refresh token is available in this mode |
| `EPOST_API_KEY` | Business mailbox only; does not support tenant discovery |

Bearer token and API key are mutually exclusive; neither may be combined with `--password-stdin`. They take precedence over password environment variables. There is no command-line password argument, automatic `.env` loading, persistent session or stored token. Password persistence is opt-in and uses the native macOS Keychain only; profile JSON rejects password and token fields. Runtime environment/stdin password input overrides the saved password source. Each invocation authenticates anew, using prompts, explicitly supplied runtime credentials or an explicitly saved Keychain password. In-session credentials stay in process memory; the program cannot guarantee erasure of every copy made by the OS or HTTP libraries.

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

`list` fetches one page by default (`--page-size 48 --offset 0`). Use `--all` to traverse from offset zero; `--offset` and `--all` cannot be combined. Traversal stops after a short/empty page or an error/safety bound. Limits are page size 1–1000, default 1000 pages, at most 100,000 entries per inventory, and at most 100,000 retrieval records per sync. Detected overlap, missing IDs, repeated pages and unselectable folders that are not explicitly reported empty without subfolders produce a partial result. A single-page command can succeed without claiming pagination completeness.

## Output and exit codes

Commands use readable text by default, including redirected output; `--json` selects compact structured JSON. Capable terminals show live Ratatui progress and restore the terminal before the lasting summary. Help and version are ordinary text. When structured output is selected, successful and partial commands write one report to stdout:

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

For reports with multiple failures, storage integrity takes precedence over authentication, then partial retrieval. Authentication loss stops further requests. Later-page errors retain previously listed metadata; sync can retrieve those letters after non-authentication page failures. Fatal errors have no normal stdout report, except a conditional sync failure after a completed inbox check: that check is retained with its redacted nested sync error. Callers should ignore unknown JSON fields and check both exit status and individual retrieval records.

Sync reports include inventory counts/issues, `pagination_complete`, individual document statuses, and `mailbox_coverage_verified: false`. Saved-document records expose path, SHA-256, byte count, media kind and original `content_type` (null if absent); repeat runs retain the original type. Optional `format_hint` describes a byte signature (PDF, PNG, JPEG, TIFF, GIF, ZIP, HTML or text) on new content and verified cached files. It does not validate or convert the format, change stored filenames or manifest records, or make non-PDF retrieval supported. Document statuses include `saved_pdf`, `saved_json`, `saved_binary`, `already_saved`, `awaiting_manual_acceptance_or_status_verification`, `failed`, and `deferred_due_to_authentication_failure`. JSON/binary content is preserved but counts as unsupported retrieval (exit 2), including on repeat runs. Pending/unknown registered-letter states remain partial even if previously cached content exists.

Save the report somewhere private and configure your automation engine to alert on **every nonzero exit**, including exit 2. A successful download alone is insufficient evidence that all mail has been noticed or acted on. Scheduling and notifications remain external; configured Paperless ingestion and verified folder copies are supported.

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

The tests use invented identifiers, fake secrets, generated bytes and literal loopback HTTP servers. Spawned CLI processes clear inherited environment variables and never load a developer's credentials; guided tests inject synthetic prompts and passwords. Credential-storage tests use fake stores and never access the host Keychain. Tests cover request forms/query encoding, private/business selection, token expiry/rotation, status handling, pagination/partial reports, registered-letter deferral, JSON/binary preservation, interrupted writes, safe repeats, corruption, path attacks, concurrent store access, secret redaction, prompt retries, numbered selection, profile compatibility, login-source selection and progress/report separation. Unix storage/profile tests are conditionally compiled. CI uses a Linux network namespace with only loopback networking available during test execution; dependency fetching happens earlier. Native Keychain behavior and provider compatibility are not established by these tests.

No provider integration suite or authenticated provider test is included. `--environment sandbox` selects only the documented test host for deliberate runtime use; it never falls back to production. Sandbox access and private-mailbox parity remain unresolved because official documentation conflicts.

## Known provider limitations and references

Offset lists expose no snapshot, total or cursor; even normal termination cannot establish a consistent complete mailbox inventory. Inbox requests include all six documented letter types; future types may need an update. Branded archive directories can have empty IDs with no documented distinct selector. Sync never treats them as root storage. An unselectable folder is recorded as `unselectable_empty` only when the provider explicitly reports zero documents and no subfolders. This is an empty observation, not a verified snapshot. Unknown, negative or positive counts, or possible subfolders, remain partial. Every selectable folder is still scanned even when its reported count is zero. The JSON `archive_directories` records each observed folder and the selection outcome so users can identify folders needing attention. Archive search supports a STORAGE scope, but omitted-keyword behavior and complete branded-folder coverage are undocumented, so it is not used to claim completeness. The content schema is ambiguous about PDF versus JSON/binary responses. Registered-letter acceptance remains a manual action.

Contract research was performed on 2026-10-11. Only the required contract notes are maintained in [docs/api-brief.txt](docs/api-brief.txt); the full provider specification is linked, not redistributed.

- [Production Swagger](https://api.epost.ch/docs) and [OpenAPI JSON](https://api.epost.ch/openapi?query=All)
- [Private tenant authentication](https://developer.epost.ch/docs/api-docs/v6nqmmjkxcery-how-to-access-the-letterbox-public-ap-is-with-a-private-tenant)
- [Inbox listing](https://developer.epost.ch/docs/api-docs/f52fd27fd878a-get-letters-of-current-user), [content](https://developer.epost.ch/docs/api-docs/f3d6c00359743-get-the-content-of-a-letter-with-given-id)
- [Archive folders](https://developer.epost.ch/docs/api-docs/048e72f82314c-get-all-folders-branded-folders-unbranded-folders-and-subfolders-in-the-storage), [archive listing](https://developer.epost.ch/docs/api-docs/c0ca4f1606215-get-letters-in-root-storage-or-in-a-specific-folder)
- [Test onboarding](https://developer.epost.ch/docs/api-docs/z6ydxfa37ns58-how-to-get-started-on-our-test-environment) and [TEST Swagger](https://api-test.klara-epost.tech/docs)

Contributor boundaries and the original implementation brief are in [AGENTS.md](AGENTS.md) and [docs/cloud-task.md](docs/cloud-task.md).
