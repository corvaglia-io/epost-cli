## epost-cli v0.2.0

Run `epost` for the Ratatui menu, or `epost check inbox` to retrieve eligible unsaved mail and resume delivery to every configured destination using the saved default profile.

- Guided setup, explicit mailbox selection, default profiles and optional local macOS Keychain credentials.
- Live terminal progress and lasting text summaries; `--json` and `--non-interactive` support automation.
- Multiple named Paperless-ngx and verified folder destinations, with independent delivery history and failure isolation. `--destination NAME` restricts one run.
- Resumable ingestion, original-byte verification and `epost verify`, which audits existing copies without submitting new PDFs. Pending delivery resumes even when the inbox is quiet.
- Custom inbox/source/extra tags and optional document-type or Text provenance mappings. Received time is preserved separately from document issue date; missing metadata is omitted.
- Read-only `epost doctor` diagnostics for storage, unattended credentials, source access and DNS/TLS/proxy routing. Access redirects are probed before loading a Paperless token.
- Legacy profile, Keychain and Paperless history migration, including safe restoration of removed legacy destinations without losing their binding.
- Read-only inbox/archive retrieval, bounded requests, atomic storage, repeat-run integrity checks and actionable public-API download errors.

Both `epost` and `epost-cli` are included. Existing profiles are reused; use `epost profile use NAME` if multiple accounts have no default. See [daily commands and migration](https://github.com/corvaglia-io/epost-cli/blob/v0.2.0/docs/commands.md) and [Paperless setup](https://github.com/corvaglia-io/epost-cli/blob/v0.2.0/docs/paperless.md).

All ePost mail operations remain read-only: downloading or delivering does not mark mail read, accept registered letters, move/archive mail, or act on document contents. Website download fallback is not implemented. This project is unofficial and offers no guarantee of provider availability, compatibility or complete mailbox coverage. Development and CI use synthetic data; native Keychain and live provider behavior still require operator testing.

Download the archive matching your operating system and CPU, verify it with `SHA256SUMS`, then extract it. Linux builds use glibc on Ubuntu 22.04 and are not static/musl builds. macOS builds are unsigned and not notarized. No Windows binary, container image or crates.io package is published. Each archive contains both executables, README.md and the MIT LICENSE.
