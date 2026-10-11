# Versioned releases

The release workflow builds archives for four native targets:

| Target | GitHub runner | Platform |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | Linux x86-64, glibc |
| `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` | Linux ARM64, glibc |
| `x86_64-apple-darwin` | `macos-15-intel` | macOS Intel |
| `aarch64-apple-darwin` | `macos-15` | macOS Apple Silicon |

Linux archives are built on Ubuntu 22.04 and require a compatible glibc environment. They are not static builds and are not intended for Alpine/musl containers. macOS archives are unsigned and not notarized; older macOS versions are unverified. No Windows binary, Docker image or crates.io publication is included. Platform labels follow the [official runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

## Downloads

Assets use names such as `epost-cli-v0.1.0-aarch64-apple-darwin.tar.gz`. Each archive contains one same-named directory with only the executable, README.md and MIT LICENSE. The GitHub release also includes SHA256SUMS for all four archives. Download your archive and SHA256SUMS into an otherwise empty directory, then verify the selected file before extraction:

```sh
shasum -a 256 --ignore-missing --check SHA256SUMS
tar -xzf epost-cli-v0.1.0-aarch64-apple-darwin.tar.gz
./epost-cli-v0.1.0-aarch64-apple-darwin/epost-cli --version
```

The command examples assume this is your selected platform; change the target for your machine. Confirm that the checksum tool reports the selected archive as OK. Checksums detect changed downloads; they are not a signature or independent proof of provenance.

## Prepare and trigger a release

The package version is already `0.1.0`. Before creating the first tag, merge the reviewed implementation and release workflow into main. Review docs/release-notes.md for the intended release. For later releases update Cargo.toml/Cargo.lock and release notes in a reviewed PR first.

Run these steps as the maintainer when ready to publish:

```sh
git switch main
git pull --ff-only
git tag -a v0.1.0 -m 'epost-cli v0.1.0'
git push origin v0.1.0
```

Tag pushes matching `v*` trigger validation. The tag must exactly equal `v` plus the package version and resolve to a commit already merged into main. Tests/formatting/Clippy run with locked dependencies, and all four native binaries must build and pass credential-free version/help smoke checks. Archives receive SHA-256 checksums. Build jobs have read-only repository permissions and no ePost secrets.

Only the tag-triggered publisher has contents write permission. It rechecks the remote tag and main ancestry, verifies all archives/checksums, creates a draft release with all assets, and publishes only after uploads complete. SemVer prerelease tags such as `v0.2.0-rc.1` become GitHub prereleases. Existing releases/assets are not overwritten. A failed upload leaves a draft requiring maintainer inspection; rerunning will refuse that existing release. Do not move release tags to retry different code.

PR runs touching release/build configuration and manual workflow runs produce CI preview artifacts, retained for seven days, without creating or publishing a GitHub release. Artifact transfer uses tar archives to preserve executable permissions, as described in the [official artifact documentation](https://github.com/actions/download-artifact#maintaining-file-permissions). The workflow's release operations use the [GitHub CLI](https://cli.github.com/manual/gh_release_create).

Release automation and synthetic tests do not establish live ePost compatibility or complete mailbox coverage. No personal/company credentials or real mail are needed for any build or release step.
