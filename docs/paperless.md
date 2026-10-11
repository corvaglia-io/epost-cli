# Paperless-ngx delivery

Paperless is an optional delivery destination. Normal ePost sync stays local unless you explicitly add `--paperless`. Delivery currently supports validated PDFs only. Local files remain in the ePost store after delivery.

## First setup

Open your Paperless user menu → **My Profile** and generate an API token. The user needs permission to view/add tags, view/add documents and view consumption tasks. Choose a user whose document ownership and visibility you want for the uploaded mail. Do not share the token in chat or commit it.

```sh
# macOS: hidden token prompt, explicitly remember it in the local Keychain
epost-cli --profile personal paperless setup \
  --url https://paperless.example.invalid --remember
# Upload the already downloaded PDFs; no ePost sign-in is required
epost-cli --profile personal paperless upload
# Later: retrieve mail, then deliver saved PDFs
epost-cli --profile personal sync --paperless
# Inspect recorded delivery state without credentials or network requests
epost-cli --profile personal paperless status
```

Use your real instance URL, including an installation path if applicable. Include the same `--config` argument used when creating your ePost profile. Configuration belongs to the profile's exact output/mailbox namespace; an `--output` override selects a separate store and requires its own Paperless setup.

Setup uses or creates **ePost Inbox** with the inbox flag, and **ePost** as a regular source tag. To use existing tags, specify `--inbox-tag ID --source-tag ID`. The source tag must not be an inbox tag. Setup never changes an existing tag's settings. It creates tags only after token authentication; a failure can leave a created tag, which a later setup reuses. Setup does not upload documents.

Paperless inbox tags provide a review queue: filter documents by ePost Inbox, optionally save that view on the dashboard, and remove the inbox tag once you have handled the letter. Successful ingestion does not mean an invoice is paid or a deadline is handled. The CLI leaves reviewed documents reviewed on repeat runs.

## Credentials and unattended use

Without `--remember`, the CLI uses the explicit `PAPERLESS_TOKEN` environment variable, a private file supplied with global `--paperless-token-file /absolute/path/token.txt`, or a hidden terminal prompt. It never takes a token as a command-line value. Token files must be regular, private, owned files with no symlink/hard-link aliases on Unix. If you rotate a token, rerun setup with the replacement; when changing from remembered credentials to runtime input, setup retires the old saved entry.

`--remember` saves only the Paperless token in the native local, nonsynchronized macOS Keychain. Its identity is bound to the store path, mailbox namespace and destination URL. The target JSON contains URL, tag IDs and credential-source preference, never a token. Keychain updates and configuration replacement share the store lock and roll back on failures before commit. `paperless logout` forgets the saved local token while retaining target and delivery history; it does not revoke the server token. Locked/unavailable Keychains require runtime credentials for unattended jobs.

An HTTPS API must be reachable with the token alone. Redirects, interactive proxy sign-in and Cloudflare Access challenges are reported rather than followed; browser cookies and Cloudflare service credentials are not supported in this version. Request and file limits can be changed with the existing global timeout/retry/size flags, up to the hard bounds.

## Delivery and recovery

The CLI verifies each local PDF, records upload intent durably, uploads it once with the configured tags, then polls the asynchronous consumption task. It confirms the original's byte count and SHA-256 through a separate download before recording delivery. New confirmations also check that the intended tags are present and that the inbox flag is still enabled. PDFs use deterministic opaque filenames for recovery; document titles/correspondents/dates are not copied from ePost metadata in this version.

Pending task IDs persist across runs. If a connection or crash loses the upload receipt, retrying looks for the exact deterministic original filename and verifies its bytes. An empty search does not prove the upload was never accepted, so uncertain uploads are never automatically submitted again. A terminal ingestion failure similarly requires reconciliation or manual correction. Existing copies with different filenames are not guessed or adopted automatically.

```sh
# Bounded first batch; later runs advance beyond already verified documents
epost-cli --profile personal paperless upload --limit 3 --wait-seconds 60
# JSON stdout and live, redacted progress on stderr
epost-cli --json --progress --profile personal paperless upload
# Adopt a manually located existing document only after original and inbox checks
epost-cli --profile personal paperless reconcile synthetic-letter-id \
  --document-id 42
```

`--limit` bounds new/resumed unresolved work; verified documents are rechecked without consuming that allowance. `--wait-seconds` bounds delays between task checks per document; HTTP request time and bounded retries are additional, so it is not a wall-clock deadline. Slow ingestion remains pending for the next run. A missing/altered remote original is attention, never silent success or an automatic replacement. Automatic delivery does not import PDFs downloaded separately through the website into the local manifest.

Local `paperless status` describes recorded state only; it does not establish current server health. Delivery reports contain counts, hashed document references, Paperless document IDs and redacted errors. Exit 0 means all supported work in that run was verified; exit 2 indicates pending/uncertain/failed/unsupported/deferred work, exit 3 authentication failure, and exit 4 local integrity failure. A combined sync retains its ePost result and adds Paperless results/errors. If ePost was already partial or failed, delivery cannot erase that status.

Delivery state is private, atomic and locked alongside the existing mailbox store. An existing ledger is bound to its destination URL and namespace; rebinding it to a different server is rejected. Instance migration, deduplication across separate stores, automatic retry of failed uploads, metadata mapping and ePost archive mutations remain outside this version.

## Contract and verification

Implementation uses the documented token-authenticated multipart `POST /api/documents/post_document/`, asynchronous task polling and canonical original download. It parses legacy unpaginated tasks and current paginated tasks; API responses, supplied URLs and metadata are bounded and treated as untrusted. GET retries are bounded; upload/tag POSTs are never replayed automatically, and redirects are disabled.

Sources: [official REST API documentation](https://docs.paperless-ngx.com/api/), [recommended inbox workflow](https://docs.paperless-ngx.com/usage/#usage-recommended-workflow), [official document/task views](https://github.com/paperless-ngx/paperless-ngx/blob/main/src/documents/views.py), [serializers](https://github.com/paperless-ngx/paperless-ngx/blob/main/src/documents/serialisers.py), and [filters](https://github.com/paperless-ngx/paperless-ngx/blob/main/src/documents/filters.py). Contract checked 2026-10-12. Your instance exposes its version-specific API viewer at `/api/schema/view/`.

All project tests use synthetic localhost services, fake credential stores and generated PDFs. No developer profiles, native Keychain, real mail or live Paperless/ePost credentials are used. These tests establish client behavior, not compatibility or ingestion health on an actual instance. Live confirmation is a separate user-run step after build/review.
