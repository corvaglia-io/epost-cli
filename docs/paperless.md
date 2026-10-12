# Paperless-ngx

Paperless is an optional delivery destination for verified PDFs. ePost mail stays
unchanged, and downloaded files remain in the local store.

## Setup

Create an API token in Paperless under **My Profile**, then run:

```sh
epost destination add main --url https://paperless.example.invalid
```

Replace the example URL with your instance. The wizard asks for the token in a
hidden prompt, offers local macOS Keychain storage, and configures tags and
optional metadata. Setup uploads no documents. The Paperless user needs access
to documents, tags and consumption tasks; creating tags or custom fields also
requires permission to add them.

The API must be reachable directly over HTTPS with token authentication. A
browser sign-in proxy requires a reachable internal route or another supported
access arrangement; browser cookies and proxy service tokens are not supported.

## Tags and classification

The wizard proposes two separate tags:

- **epost**: a regular source tag to keep on the document.
- **inbox**: an inbox tag to remove after reviewing and handling the document.

Custom names and additional tags are supported. Renaming an existing tag in
Paperless preserves its ID and document assignments. Repeat runs preserve tags
you removed during review, including `inbox`.

The default omits a document type so Paperless can classify the upload using its
configured matching rules. You can instead select an existing fixed type or map
ePost's semantic document types to existing Paperless type IDs. Ambiguous or
missing mappings are omitted. Types must already exist in Paperless.

Optional **Text** custom fields can retain the ePost ID, received timestamp,
original filename, raw document types or a static value. Received time is not
used as the document's issue date. Available letter titles are copied; sender
identifiers are not converted into correspondents. Tag and metadata settings
apply to new uploads.

See Paperless's [matching guide](https://docs.paperless-ngx.com/advanced_usage/#automatic-matching)
and [review workflow](https://docs.paperless-ngx.com/usage/#usage-recommended-workflow).

## Daily use

```sh
epost check inbox                  # Retrieve and deliver to every destination
epost deliver --limit 3             # Deliver a small batch of saved PDFs
epost deliver --destination main    # Resume only this destination
epost verify --destination main     # Audit existing copies without uploading
epost destination edit main         # Change setup or replace its token
```

All configured destinations are used by default. `--destination` restricts one
run. Configuration and delivery history belong to the selected profile and
local store; use the same `--profile`, `--config` and output location each time.

For unattended use after saving credentials:

```sh
epost --non-interactive --json check inbox
```

Saved credentials must be accessible without a Keychain approval dialog.
Runtime token input is also supported: `PAPERLESS_TOKEN` or a private file passed
with `--paperless-token-file` when there is one Paperless instance. Multiple
instances need separately saved tokens or per-destination variables named
`EPOST_DESTINATION_<UPPERCASE_HEX_OF_UTF8_NAME>_TOKEN`; for `main`, this is
`EPOST_DESTINATION_6D61696E_TOKEN`. Tokens are never command-line values or saved
in profile JSON.

## Recovery

Delivery is confirmed after ingestion finishes and the downloaded original
matches the local PDF's byte count and SHA-256. Initial tags are checked for new
uploads. Ordinary repeat runs use recorded confirmations; `verify` performs a
fresh audit.

Run `epost deliver` again to resume pending tasks. An uncertain upload is checked
without automatically posting it again. When Paperless reports a duplicate,
the existing original is adopted only if its bytes match; its tags and reviewed
state are preserved.

To reconcile a document you located manually:

```sh
epost reconcile main example-letter-id --document-id 42
```

Use the saved ePost letter ID and the corresponding Paperless document ID. This
verifies the original, without uploading or changing server metadata. Failed or
unsupported items remain visible; exit `2` means attention is needed.

Removing a destination keeps local documents and delivery history. Uploading
does not mean a letter's payment, deadline or other required action is handled.
See the [command reference](commands.md) for status codes and legacy commands,
and the [Paperless API documentation](https://docs.paperless-ngx.com/api/) for
server behavior.
