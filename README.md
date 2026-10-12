# epost-cli

I created this Rust CLI for personal use and thought it might be useful to others.
It downloads documents from an ePost inbox or archive and can deliver them to
Paperless-ngx or local folders.

This is an unofficial project, provided as-is under the [MIT license](LICENSE).
It is **not actively maintained**. Please review the code—or have it reviewed—
before trusting it with your mail. Fork it if you want to change or maintain it.
It is not affiliated with ePost, KLARA or Swiss Post.

## Install

Download the macOS or Linux archive for your processor from
[GitHub Releases](https://github.com/corvaglia-io/epost-cli/releases), verify its
checksum, extract it and put `epost` on your PATH. Alternatively, with Rust 1.98
or newer:

```sh
git clone https://github.com/corvaglia-io/epost-cli.git
cd epost-cli
cargo install --path . --locked
```

Both `epost` and `epost-cli` run the same tool. See the
[installation and usage guide](https://github.com/corvaglia-io/epost-cli/blob/main/docs/getting-started.md)
for binary installation, onboarding and automation.

## Use

```sh
epost                         # terminal menu and guided setup
epost destination add         # optional Paperless or folder destination
epost doctor                  # check configuration and access
epost check inbox             # fetch mail and deliver to all destinations
```

Setup saves the selected mailbox and document folder. On macOS, it can also save
credentials in your local Keychain. Private API login uses an **ePost/KLARA
password**, separate from your SwissID password. Keep the same document folder:
repeat runs verify and reuse saved files.

After setup and Keychain permission grants, the unattended command is:

```sh
epost --non-interactive --json check inbox
```

All configured destinations are used by default. Terminal commands show progress
and a summary; `--json` provides structured output for scripts. The
[command reference](https://github.com/corvaglia-io/epost-cli/blob/main/docs/commands.md)
covers archive retrieval, profiles and destination selection.

## Limits

- ePost operations are read-only: downloading never marks mail read, accepts
  registered letters or moves it into the archive.
- Some content or archive folders may be unavailable through the public API.
  Check nonzero exits and compare results with ePost; complete coverage is not
  guaranteed.
- Paperless delivery supports PDFs only. New imports use separate `epost` and
  `inbox` tags by default; removing `inbox` after review stays respected.
- Tests use synthetic services. Reports, profiles and downloaded documents are
  private data and should stay out of public repositories and logs.

See [Paperless setup](https://github.com/corvaglia-io/epost-cli/blob/main/docs/paperless.md)
and [contributor instructions](AGENTS.md) for more.
