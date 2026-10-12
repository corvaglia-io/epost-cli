# Command reference

`epost` and `epost-cli` are equivalent. Start with
[installation and usage](getting-started.md); use `epost COMMAND --help` for every
option.

## Mail and delivery

| Command | Action |
| --- | --- |
| `epost` | Open the terminal menu and guided setup. |
| `epost check inbox` | Retrieve inbox documents and resume delivery to all configured destinations. |
| `epost check archive` | Retrieve existing archive root/folders and deliver. Does not move mail. |
| `epost check all` | Retrieve inbox and archive, then deliver. |
| `epost status inbox` | Observe unread/new counts and local delivery history without fetching documents or uploading. |
| `epost status archive` | Observe archive metadata and local history. |
| `epost browse inbox` | Browse metadata and download documents by number. |
| `epost browse archive` | Browse existing archive content. |
| `epost deliver` | Resume saved-PDF delivery without an ePost login. |
| `epost verify` | Audit recorded destination copies against local bytes; no new uploads. |
| `epost reconcile main example-letter --document-id 42` | Adopt a selected Paperless original only if its bytes match the saved PDF. No upload or tag changes. |
| `epost doctor` | Check local state, unattended credentials and source/destination access. |
| `epost doctor --offline` | Local checks only; no network or Keychain reads. |

Every configured destination participates by default. `check` also resumes
existing delivery work when the inbox has no new mail. Read/unread status is
independent of whether a local file is saved; nothing marks mail read.

```sh
epost check inbox --destination main                 # select one destination
epost check inbox --destination main --destination backup
epost check inbox --local                            # local retrieval only
epost check inbox --limit 3                          # small first batch
epost --profile business check all                   # select an account
```

`--limit` bounds new content attempts and unresolved delivery work **per destination**,
including pending-task checks. Saved-file reuse and confirmed-delivery reuse do
not consume their respective limits. `--output` selects a different
source store, with separate destination settings and history.

## Setup and credentials

| Command | Action |
| --- | --- |
| `epost setup --profile personal` | Choose mailbox, document folder and credential storage. |
| `epost profile list` | List saved profiles. |
| `epost profile use personal` | Choose the default account. |
| `epost login --remember` | Verify login and save the password in the local macOS Keychain. |
| `epost login` | Verify login and remember email only; prompt for the password later. |
| `epost logout` | Forget this profile's saved Keychain password; retain mailbox settings. |
| `epost destination add` | Guided Paperless or folder setup. |
| `epost destination add backup --folder /absolute/private/backup` | Add a verified local-copy destination. |
| `epost destination list` | List configured destinations. |
| `epost destination edit main` | Edit Paperless tags/metadata and token preference, retaining history. |
| `epost destination remove main` | Remove configuration/token with confirmation; retain documents and history. |

Profiles store settings, optional email and credential-source preferences, never
passwords or tokens. Default config: `~/Library/Application Support/epost-cli/config.json`
on macOS; `$XDG_CONFIG_HOME/epost-cli/config.json` or `~/.config/epost-cli/config.json`
on Linux. Global `--config /absolute/path/config.json` selects another file.
Changing profile identity or config location can invalidate its Keychain binding.

### Authentication inputs

| Input | Use |
| --- | --- |
| Saved macOS Keychain password | Used after explicit opt-in and permission grant. |
| `EPOST_USERNAME` + `EPOST_PASSWORD` | Runtime password login; the remembered profile email can replace `EPOST_USERNAME`. |
| `--password-stdin` | One password line from stdin; needs a username or remembered email. |
| `EPOST_BEARER_TOKEN` | Existing token; no refresh in this mode. |
| `EPOST_API_KEY` | Business mailbox only; no tenant discovery. |
| `PAPERLESS_TOKEN` | Runtime token when only one Paperless destination is configured. |
| `EPOST_DESTINATION_<HEX_NAME>_TOKEN` | Named token; `<HEX_NAME>` is the uppercase hexadecimal encoding of the destination name's UTF-8 bytes (`main` becomes `6D61696E`). |
| `--paperless-token-file /absolute/private/token` | Private token file when only one Paperless destination is configured. |

Bearer/API-key modes are mutually exclusive and cannot be combined with password
stdin. With multiple Paperless destinations, use separately saved Keychain tokens
or named runtime variables even when selecting one destination for a run. Runtime
ePost password input overrides the saved password source. There
are no password/token command-line values or automatic `.env` loading. Do not
commit runtime secrets, profiles or reports.

## Display and unattended runs

```sh
epost --non-interactive --json check inbox
epost --non-interactive --json doctor
epost --details check inbox
epost --no-progress check inbox
```

Normal terminals show progress and a text summary; the menu keeps results open.
`--json` emits structured results and disables the UI. `--progress --json` adds
plain progress on stderr. Redirected output is plain text. `--non-interactive`
forbids prompts and native approval dialogs; missing credentials produce an error.
Setup, browse and credential changes requiring approval need an interactive terminal.

Exit codes: **0** completed, **1** configuration/transport/I/O failure, **2** partial
or unsupported work (also invalid usage), **3** authentication failure, **4** integrity
or store-lock failure. Mixed results retain individual issues and a nonzero exit.
JSON reports have a `schema_version` and may contain private mail metadata.

## Lower-level commands

The original commands remain available for explicit API use:

```sh
epost --json tenants
epost inbox list --all
epost archive folders
epost archive list --root --all
epost show example-letter
epost download example-letter --output /absolute/private/store
epost sync --scope both
```

Select a profile or provide `--tenant-kind private|business`, `--tenant-id` and
`--company-id` from discovery. Without a profile, download/sync also need
`--output`. Lists fetch one page unless `--all` is supplied. `--page-size`,
`--offset` and `--max-pages` control pagination. `sync` retrieves locally unless
`--paperless` is explicitly selected; use `check` for all configured destinations.

Legacy Paperless configuration is retained as destination `paperless`, including
existing tag IDs and delivery history. Once named destinations are saved, use
`destination edit/remove` and `check/deliver` rather than legacy `paperless` setup.
