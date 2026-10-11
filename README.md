# epost-cli

An unofficial Rust CLI and library for retrieving documents from an ePost inbox and archive.

Status: implementation brief prepared; Rust implementation has not started. This repository is independent of ePost, KLARA and Swiss Post. No guarantee of API compatibility or complete mail coverage is provided.

## Initial scope

- Authentication and explicit selection of a private or business mailbox.
- Inbox letter listing and document downloads.
- Archive folder enumeration, root archive listing and archived document downloads.
- Pagination, token refresh, bounded retries and safe repeated retrieval.
- Structured output and meaningful exit codes for scripts and agents.

Mail operations are read-only. Sending, accepting or rejecting registered letters, marking letters read, moving, uploading and deleting documents are outside the initial scope. Authentication POST requests are necessary for access.

## Development and testing

Development and default CI use invented account identifiers, synthetic response fixtures, generated documents and localhost mock servers. No personal or company ePost credentials, real mail, production response recordings or private PoC downloads belong in this repository or its tests.

An optional provider integration suite may use a dedicated sandbox account containing synthetic documents if access is confirmed. Sandbox availability and parity with private mailboxes remain unverified: official onboarding describes customer test tenants, while the current TEST Swagger labels the deployment internal.

See [the cloud implementation task](docs/cloud-task.md) and [contributor instructions](AGENTS.md).

## Public API references

- [Production Swagger](https://api.epost.ch/docs)
- [OpenAPI JSON](https://api.epost.ch/openapi?query=All)
- [Private tenant authentication guide](https://developer.epost.ch/docs/api-docs/v6nqmmjkxcery-how-to-access-the-letterbox-public-ap-is-with-a-private-tenant)
- [Test onboarding](https://developer.epost.ch/docs/api-docs/z6ydxfa37ns58-how-to-get-started-on-our-test-environment)
- [TEST Swagger](https://api-test.klara-epost.tech/docs)

The provider's schema contains APIs beyond this project's scope. Only the inbox, archive and supporting authentication contracts are relevant here. The provider schema is linked, not redistributed.

License selection is pending. No package or binary release has been published.
