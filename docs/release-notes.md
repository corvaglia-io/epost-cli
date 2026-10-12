# v0.2.1

The menu now keeps a readable result screen after progress finishes, with
separate source/destination counts, grouped issues and explicit next steps.
Enter returns to the menu, `r` retries, `g` runs Doctor, `d` toggles details,
and `q`/Esc quits. Standalone commands keep compact text; `--details` expands
technical diagnostics and `--json` retains structured reports.

Paperless duplicate failures are recognized separately from uncertain uploads.
A referenced existing original is adopted only after byte-count and SHA-256
verification. Its tags, type and reviewed state are preserved. Failed tasks
from older journals are rechecked without re-uploading. Unknown, conflicting
or mismatching results remain attention with a specific recovery action.
`epost reconcile DESTINATION LETTER_ID --document-id ID` supports explicit
original-byte verification against a manually selected existing document.

New Paperless setup proposes separate `epost` and `inbox` tags. Existing tag IDs
and credential/history bindings are preserved. The setup guide explains optional
document types, Auto matching and removal of inbox tags after human review.
Source Status counts remain visible alongside destination history, and cached
non-PDF files are counted separately from reused PDFs.

Validation uses synthetic localhost APIs and fake credentials only. Provider
compatibility and complete mailbox coverage remain unverified. The documented
ePost content-404/manual-download limitation remains visible; browser fallback
is still a future improvement. No ePost mailbox mutation is introduced.
