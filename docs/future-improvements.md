# Possible future improvements

These are candidates, not implemented features or commitments.

## Optional ePost archiving after verified Paperless delivery

Paperless delivery is implemented separately from ePost retrieval. It verifies
ingestion, original bytes and initial review tags, while ePost remains read-only.
The local delivery ledger already prevents routine repeat uploads; archiving
ePost letters is not required for that purpose.

A future explicit opt-in could archive a letter only after its destination copy
is verified again and the intended mailbox is confirmed. It would need a reviewed
provider mutation contract, registered-letter safeguards, durable reconciliation
after ambiguous mutation responses, and independent synthetic tests. The policy
must distinguish stored documents from reviewed/handled documents; delivery does
not mean an invoice was paid or a deadline was handled. Prefer a separate command
with a preview and explicit policy rather than changing normal sync behavior.

Named destinations and selected optional metadata mappings are implemented.
Remaining candidates are migration of documents between instances, controlled
resubmission of terminal failed uploads, and optional Cloudflare Access
service-token authentication. These remain unimplemented.

## Authenticated website download fallback

An item can have accessible metadata while the public content endpoint returns
HTTP 404. The CLI detects this as `content_unavailable_metadata_available`, keeps
the retrieval failed, and recommends checking or downloading it in ePost.
That diagnostic alone cannot establish whether a website download will succeed.

A candidate fallback route is:

```text
GET https://app.epost.ch/luz/api/epost-storage/downloads/letters/{letter-id}
```

This is an observed website route, not part of the documented public API contract.
It can work within an authenticated website session. Do not assume a public API
bearer token authenticates that session, or that the route is stable or supports
every item. No browser session, production recording or document is included in
the repository or tests.

A future implementation could use deterministic requests in an explicitly
authorized browser session after public API retrieval fails. It would need:

- Explicit opt-in and account/mailbox binding, with a controlled provider origin
  and redirects. Do not send public API credentials to an arbitrary reference URL.
- Session-expiry detection and a clear reauthentication alert; never treat a login
  page, redirect or missing session as a downloaded file or a quiet inbox.
- The existing registered-mail deferral, bounded reads, PDF/content validation,
  atomic storage and repeat-run integrity checks.
- Synthetic local tests for successful retrieval, wrong-account protection,
  expired sessions, redirects, invalid content and failed retries. Runtime browser
  use must remain separate from project development and CI.

The current CLI performs no website fallback and does not automatically import
files downloaded separately through the website or mobile app.
