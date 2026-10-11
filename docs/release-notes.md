Development release of the unofficial epost-cli Rust CLI and library.

- Guided setup with retrying prompts and explicit numbered mailbox selection.
- Readable inbox/archive browsing and numbered downloads with one sign-in per session.
- Private profiles for short download/sync commands, with remembered usernames and explicit macOS Keychain password opt-in.
- Repeat syncs verify saved files and fetch only unsaved content.
- Read-only inbox checks for unread and locally unsaved items, plus an optional conditional inbox sync and a single-request unread-only mode.
- Live terminal sync progress and a single readable summary, including cached non-PDF content, with JSON reports retained for automation.
- Content 404 diagnostics using one canonical read-only metadata check, and byte-derived format hints for saved and cached content.
- Clear public-API download errors and a JSON recovery action when matching metadata remains accessible; manual ePost download guidance preserves failed status and partial exit codes. Authenticated website fallback is documented as a possible future improvement only.
- Archive folder observations identify unsupported selectors; explicitly empty folders without subfolders are informational, with coverage still unverified.
- Profile login updates and local password forgetting without repeating mailbox setup.
- Optional Paperless-ngx setup and PDF delivery, with separate runtime/Keychain token input, inbox/source tags, ingestion progress, resumable tasks, original-byte verification and checked reconciliation. `sync --paperless` adds delivery without changing ePost mail. Live Paperless compatibility remains unverified.
- Paperless redirects produce redacted access/routing guidance and stop a delivery batch before attempting further documents; redirect URLs and query values are never logged or followed.

- Read-only inbox and archive metadata/document retrieval.
- Explicit private/business mailbox selection, password/bearer authentication and business API keys.
- In-session token refresh, bounded pagination/retries and JSON reports with nonzero failure exits.
- Atomic local storage with mailbox namespaces, hashes, locking and repeat-run verification.

Development and CI use synthetic data only. Provider compatibility and complete mailbox coverage remain unverified. Registered letters requiring acceptance remain pending; downloading a document does not act on its contents. See the included README for command examples and limitations.

Choose the archive matching your operating system and CPU architecture. Verify it using SHA256SUMS before extracting. Linux binaries use glibc and are built on Ubuntu 22.04; they are not static/musl builds. macOS binaries are unsigned and not notarized. No Windows binary, container image or crates.io package is included. Each archive contains epost-cli, README.md and the MIT LICENSE.
