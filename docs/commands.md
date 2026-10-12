# Daily commands and terminal UI

Both `epost` and `epost-cli` run the same implementation. Install with
`cargo install --path . --locked`; Cargo's binary directory must be on PATH.
Release archives include both names. No daemon or scheduler is installed.

## First use

```sh
epost                         # home menu and guided setup
epost setup --profile personal
epost profile list
epost profile use personal     # default account for short commands
epost destination add          # guided Paperless or folder setup
epost doctor
epost check inbox
```

Setup still requires an explicit private/business choice and tenant selection.
It remembers the username and offers local macOS Keychain password storage.
Choose that option affirmatively. It stores no authenticated session. Grant
Keychain access to the executable you intend to use, then run Doctor to check
unattended access. A newly built binary may need access granted again.

The first saved profile becomes the default. A legacy file containing exactly
one profile works without rewriting its credential identity. Multiple legacy
profiles require `profile use NAME` or an explicit `--profile NAME`; the CLI
never guesses the account. All commands accept global `--config` and `--profile`.

## Command map

| Command | Behavior |
| --- | --- |
| `epost` | Home menu: Check inbox, Inbox, Archive, Deliver, Problems, Doctor, Destinations, Settings. |
| `epost check inbox` | Scan inbox, verify/reuse local files, fetch eligible unsaved content, then resume delivery to **every configured destination**, even if the inbox is quiet. |
| `epost check archive` | Same routine for existing archive root/folders. Does not move mail. |
| `epost check all` | Retrieve inbox and archive, then deliver. |
| `epost check inbox --local` | Fetch/reuse local files without destination delivery. |
| `epost check inbox --destination main` | Restrict this run to `main`. Repeat the option to select several destinations. |
| `epost check inbox --limit 3` | At most three new content attempts and three new submissions per destination. Existing saved files do not consume the content-attempt limit. |
| `epost status inbox` | Observe unread/new counts and local destination histories, without downloading content or uploading. |
| `epost status archive` | Observe archive metadata and local histories. |
| `epost browse inbox` / `epost browse archive` | Interactive metadata pages and numbered download selection. |
| `epost deliver` | Resume delivery of saved PDFs without an ePost login. |
| `epost verify` | Audit existing recorded destination copies and local bytes; never upload an unsubmitted PDF or create a new folder copy. |
| `epost reconcile main synthetic-letter-id --document-id 42` | Explicitly adopt an existing Paperless original after its bytes match the saved PDF. No source login, upload or tag changes. |
| `epost doctor` | Read-only diagnostics for local integrity, credentials without prompts, source access, DNS/TLS/proxy routing and destination catalogs. |
| `epost doctor --offline` | Check local state only; no network or Keychain access. |
| `epost destination add` | Guided destination setup. |
| `epost destination add backup --folder /absolute/private/path` | Configure a local verified-copy store without a wizard. |
| `epost destination list` | List every configured destination. All participate by default. |
| `epost destination edit main` | Configure Paperless tags, type/field mappings and token storage, retaining delivery history. |
| `epost destination remove main` | Remove configuration/token with confirmation; retain documents and history. `--yes` skips confirmation. Remembered-token removal requires interactive mode for Keychain changes. |
| `epost profile list` / `epost profile use personal` | Inspect profiles or explicitly select a default. |
| `epost login --remember` / `epost logout` | Verify/save or forget the default profile's local ePost password. |

`--output` selects a different source store for check/status/deliver/verify;
its destinations and history are separate. A failed destination cannot erase
another destination's results. A source sign-in failure can still allow delivery
of previously saved PDFs; the final run reports that source failure. Integrity failures are reported; unverifiable document bytes are withheld from
delivery. A structural source-store failure prevents normal work.

Check does not use unread count as its sole trigger: read mail may still be
unsaved, and pending ingestion needs work when no new mail arrives. Nothing
marks mail read. API pagination remains an observation, not proof of complete
mailbox coverage. Registered letters needing acceptance require action in ePost.

## Display and automation

On a capable terminal, commands show Ratatui progress, restore the terminal,
print a compact text summary and exit. In the menu, a lasting result screen
separates retrieval, destinations and issues, with a next step for each problem.
Enter returns to the menu; `r` explicitly retries, `g` opens Doctor, `d` shows
technical details, and `q`/Esc quits. Up/down and Page Up/Down scroll long results.
Retries re-read credentials and resume existing ingestion without blindly reposting.
Setup and browse use the same validation logic with Ratatui prompts. Secrets
are masked and never added to display history. `q`/Esc during progress requests
a stop after the current request and durable bookkeeping; it does not interrupt
an in-flight upload and lose its task reference.

Redirected output and `TERM=dumb` use plain text; no alternate screen or ANSI
escapes. `--no-progress` suppresses progress. `--json` emits the structured result
and suppresses the TUI/default progress; explicit `--progress` can still provide
plain stderr diagnostics. Interactive configuration and browse reject `--json`.
Bare `epost` in a pipe displays help without opening account configuration.
Use global `--details` for expanded standalone text diagnostics; `--json` retains
the complete structured report. Normal summaries count cached non-PDF content
separately from reused PDFs, rather than counting it twice.

```sh
# For Dagu, n8n, launchd or another scheduler after setup and permission grants:
epost --non-interactive --json check inbox
# A second account and selected destination:
epost --profile business --non-interactive --json check inbox --destination main
```

`--non-interactive` forbids prompts and native Keychain approval dialogs.
Credential-changing operations involving Keychain refuse this flag rather than opening approval dialogs; folder/runtime-only configuration removal remains available with `--yes`. A missing/denied credential is an error, never a silent quiet inbox. Doctor tests
that same unattended path. Exit 0 means the requested operation finished;
partial retrieval, pending/uncertain work, deferred items or diagnosis needing
attention produce a nonzero exit. See README for the existing exit-code contract.

## Migration and compatibility

The original inbox/archive/show/download/sync and `paperless` commands remain
available. `sync` remains local unless `--paperless` was explicitly supplied.
The new `check` routine uses all configured destinations automatically.

A legacy `paperless-target.json` is exposed as destination `paperless`, retaining
its existing tags, exact Keychain binding and `paperless-delivery.json` history.
In particular, **ePost Inbox is retained**, rather than silently renamed to Inbox.
Routine delivery reuses previously confirmed ingestion without downloading the
remote original again. That is explicitly reported as **previously confirmed**;
`verify` performs a fresh audit.

Adding/editing/removing a named destination materializes `destinations.json`.
Thereafter use `destination edit/remove`, `deliver`, `check` and `doctor`; old
Paperless setup/upload/logout commands refuse with guidance so two configuration
files cannot drift. The old state/history is retained. Reusing a destination name
for another endpoint is rejected while its pinned history exists; use a new name.
