# v0.2.1

- Progress dashboard with workflow steps, scoped progress bars and separate
  retrieval/destination counters.
- Result screens with grouped issues, next steps, details and retry controls.
- Paperless duplicates recovered by verifying an existing original's bytes,
  preserving its tags and reviewed state.
- Explicit recovery with `epost reconcile DESTINATION LETTER_ID --document-id ID`.
- Separate `epost` and `inbox` tags proposed for new Paperless destinations.
- Shorter setup and command documentation.
- Prepared Paperless credentials are bound to their configuration; changed
  destinations require a fresh operation before any authenticated request.

ePost remains read-only. Files unavailable through the public content API still
require manual download; website fallback is not implemented. Validation uses
synthetic local APIs and fake credentials.
