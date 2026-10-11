Initial development release of the unofficial epost-cli Rust CLI and library.

- Read-only inbox and archive metadata/document retrieval.
- Explicit private/business mailbox selection, password/bearer authentication and business API keys.
- In-session token refresh, bounded pagination/retries and JSON reports with nonzero failure exits.
- Atomic local storage with mailbox namespaces, hashes, locking and repeat-run verification.

Development and CI use synthetic data only. Provider compatibility and complete mailbox coverage remain unverified. Registered letters requiring acceptance remain pending; downloading a document does not act on its contents. See the included README for command examples and limitations.

Choose the archive matching your operating system and CPU architecture. Verify it using SHA256SUMS before extracting. Linux binaries use glibc and are built on Ubuntu 22.04; they are not static/musl builds. macOS binaries are unsigned and not notarized. No Windows binary, container image or crates.io package is included. Each archive contains epost-cli, README.md and the MIT LICENSE.
