# Releases

For installation, see [Getting started](getting-started.md).

Release CI builds these native targets:

| Platform | Target |
| --- | --- |
| Linux x86-64 | `x86_64-unknown-linux-gnu` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple Silicon | `aarch64-apple-darwin` |

Linux builds use Ubuntu 22.04 and require compatible glibc; Alpine/musl is not
supported. macOS builds are unsigned and not notarized. There are no Windows
binaries, container images or crates.io releases.

Archives contain `epost`, its `epost-cli` alias, README and MIT LICENSE. Each
release includes `SHA256SUMS` to verify the downloads.

## Publishing

Update `Cargo.toml`, `Cargo.lock` and [release notes](release-notes.md) together.
Review and merge the changes into `main`, then tag that commit with `v` plus the
exact package version. For example, for version `0.2.1`:

```sh
git switch main
git pull --ff-only
git tag -a v0.2.1 -m 'epost-cli v0.2.1'
git push origin v0.2.1
```

CI validates the tag/version and main ancestry, runs formatting, Clippy and
synthetic tests, and builds all four archives. Only the tag publisher has
repository write permission. It verifies the assets, creates a draft release,
and publishes after every upload succeeds. Prerelease tags produce GitHub
prereleases.

Existing releases are not overwritten. If publishing fails, inspect any draft
before retrying; do not move a release tag to different code. Manual workflow
runs and qualifying PRs create preview artifacts without publishing a release.
Builds and tests require no ePost or Paperless credentials.
