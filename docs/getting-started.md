# Installation and usage

## Install a release

Choose an archive from [GitHub Releases](https://github.com/corvaglia-io/epost-cli/releases):

| System | Archive suffix |
| --- | --- |
| macOS, Apple Silicon | `aarch64-apple-darwin` |
| macOS, Intel | `x86_64-apple-darwin` |
| Linux, ARM64 | `aarch64-unknown-linux-gnu` |
| Linux, x86-64 | `x86_64-unknown-linux-gnu` |

Download your archive and `SHA256SUMS` into a new directory. Replace `VERSION`
and the platform suffix below with your chosen download:

```sh
archive=epost-cli-vVERSION-aarch64-apple-darwin.tar.gz
awk -v file="$archive" '$2 == file' SHA256SUMS > selected.sha256
shasum -a 256 -c selected.sha256
```

On Linux, use `sha256sum -c selected.sha256` for the last command. Continue only
if the result names your archive and reports `OK`:

```sh
tar -xzf "$archive"
cd "${archive%.tar.gz}"
```

Run the extracted executable:

```sh
./epost --version
./epost
```

To install for your user, copy `epost` to `~/.local/bin` and put that directory
on your shell's PATH:

```sh
mkdir -p "$HOME/.local/bin"
cp ./epost "$HOME/.local/bin/epost"
chmod 755 "$HOME/.local/bin/epost"
export PATH="$HOME/.local/bin:$PATH"
```

Add the `export PATH` line to your shell configuration for future terminals.
The release also includes the equivalent `epost-cli` executable. macOS binaries
are unsigned and not notarized. Linux binaries require glibc (Ubuntu 22.04 or
compatible); they are not Alpine/static builds. Windows releases are not provided.

## Build from source

Install [Rust](https://rustup.rs/) 1.98 or newer, then:

```sh
git clone https://github.com/corvaglia-io/epost-cli.git
cd epost-cli
cargo install --path . --locked
epost --version
```

Cargo's binary directory (normally `~/.cargo/bin`) must be on PATH. The repository
pins its development toolchain in `rust-toolchain.toml`.

## Set up a mailbox

Run `epost` and follow guided setup, or start it directly:

```sh
epost setup --profile personal
```

Choose private or business, explicitly select your mailbox, and choose a private
local document folder. Setup remembers the email and mailbox settings. The first
profile becomes the default; select another with `epost profile use NAME`.

For a private account, first follow ePost's
[private API onboarding guide](https://developer.epost.ch/docs/api-docs/v6nqmmjkxcery-how-to-access-the-letterbox-public-ap-is-with-a-private-tenant).
The API uses your email and a separate **ePost/KLARA password**, not your SwissID
password. The tool does not automate browser sign-in or password setup.

On macOS, choose Keychain storage to avoid entering your password each run.
Grant access to the installed executable when macOS asks. Without saved
credentials, interactive runs prompt again. To save credentials later:

```sh
epost login --remember
```

Keep the binary at a stable location. Replacing it may require a new Keychain
permission grant; run an interactive check before relying on unattended use.

## Add destinations

```sh
epost destination add
epost destination list
```

The wizard supports Paperless-ngx and verified copies into a local folder.
Paperless needs its instance URL and an API token from your Paperless profile;
enter the token in the hidden prompt. On macOS you can opt into local Keychain
storage. See [Paperless setup](paperless.md) for tags and optional metadata.

To add a folder directly:

```sh
epost destination add backup --folder /absolute/private/backup
```

All destinations configured for the selected profile's document store participate
by default. Destination delivery retains the local source files and leaves ePost
mail unchanged.

## Daily use

```sh
epost doctor                  # diagnostics, including unattended credential access
epost check inbox             # retrieve inbox and deliver to all destinations
epost check all               # retrieve inbox and existing archive, then deliver
epost deliver                 # deliver saved PDFs without signing in to ePost
epost status inbox            # inspect counts without downloading or uploading
```

Repeated checks reuse locally verified files and resume delivery. They do not
restore an `inbox` tag you removed in Paperless. Keep the same document store so
the tool retains its retrieval and delivery history.

In the menu, results stay open until you choose an action. Enter returns to the
menu; `d` shows details, `r` retries, `g` opens Doctor, and `q`/Esc quits. During
progress, `d` switches between the dashboard and detailed activity. Scanning has
no known total; progress bars cover the current observed batch.

## Unattended use

After onboarding, saving credentials and granting access, test:

```sh
epost --non-interactive --json doctor
epost --non-interactive --json check inbox
```

`--non-interactive` forbids input prompts and Keychain approval dialogs.
`--json` disables the terminal UI. Configure your own scheduler to run the second
command and inspect its exit status; the tool does not install a scheduler.

A local macOS job needs an accessible, unlocked login Keychain and permission for
the executable it runs. On other hosts, provide runtime credentials through your
scheduler's secret mechanism: `EPOST_USERNAME` and `EPOST_PASSWORD`, or an existing
`EPOST_BEARER_TOKEN`; business accounts can use `EPOST_API_KEY`. Paperless supports
`PAPERLESS_TOKEN` when only one Paperless destination is configured, destination-specific
variables, or `--paperless-token-file`. See [authentication inputs](commands.md#authentication-inputs)
for selection rules. Keychain credentials do not travel with a copied profile.

Profiles, JSON reports and document stores contain private data. Keep them outside
this repository and avoid sending detailed JSON reports to public logs.

## When something needs attention

| Exit | Meaning |
| --- | --- |
| `0` | Requested operation completed |
| `1` | Configuration, transport, response or local I/O error |
| `2` | Partial, unsupported or unfinished work; also invalid command usage |
| `3` | Authentication failed |
| `4` | Storage integrity problem or conflicting store access |

Run `epost doctor` and read the result's next steps. A public-API content 404 with
accessible metadata means the tool cannot retrieve that file through its API
route; check/download it in ePost. Registered letters may require manual acceptance.
Non-PDF files are retained locally but are not delivered to Paperless.

Pending Paperless ingestion can be resumed with `epost deliver`. An uncertain
upload is not automatically resubmitted; inspect Paperless and use explicit
[reconciliation](paperless.md#recovery) when needed. Do not delete local history to
force a retry. Even exit 0 does not prove that the provider exposed all mail.

For additional commands and flags, see the [command reference](commands.md).
