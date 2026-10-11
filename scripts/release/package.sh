#!/usr/bin/env bash
# Package a previously built binary. This script never fetches data or credentials.
set -euo pipefail
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
export LC_ALL=C

fail() {
    printf 'package: %s\n' "$1" >&2
    exit 1
}

if [[ $# -ne 4 ]]; then
    fail 'usage: package.sh VERSION TARGET BINARY_PATH OUTPUT_DIR'
fi

version=$1
target=$2
binary=$3
output=$4

# SemVer 2.0.0, including numeric prerelease identifiers without leading zeros.
number='(0|[1-9][0-9]*)'
prerelease='(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
build='[0-9A-Za-z-]+'
semver="^${number}\\.${number}\\.${number}(-${prerelease}(\\.${prerelease})*)?(\\+${build}(\\.${build})*)?$"
if [[ ${#version} -gt 128 || ! $version =~ $semver ]]; then
    fail 'VERSION must be a path-safe SemVer without a v prefix'
fi

case "$target" in
    x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
    *) fail 'unsupported target' ;;
esac

[[ -f "$binary" && ! -L "$binary" && -x "$binary" ]] ||
    fail 'binary must be a regular executable file, not a symbolic link'
[[ -n "$output" && ! -L "$output" ]] ||
    fail 'output directory must not be empty or a symbolic link'
case "$binary" in
    /*) ;;
    *) binary="$PWD/$binary" ;;
esac
binary="$(cd "$(dirname "$binary")" && pwd -P)/$(basename "$binary")"
case "$output" in
    /*) ;;
    *) output="$PWD/$output" ;;
esac

repository=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
for document in LICENSE README.md; do
    [[ -f "$repository/$document" && ! -L "$repository/$document" ]] ||
        fail 'repository LICENSE and README.md must be regular files'
done

# Smoke commands start with an empty environment and receive no stdin. They must
# be local-only CLI paths; no account configuration or developer secrets survive.
if ! actual_version=$(env -i PATH=/usr/bin:/bin "$binary" --version </dev/null 2>/dev/null); then
    fail 'binary --version failed'
fi
[[ "$actual_version" == "epost-cli $version" ]] ||
    fail 'binary version does not match VERSION'
if ! env -i PATH=/usr/bin:/bin "$binary" --help </dev/null >/dev/null 2>/dev/null; then
    fail 'binary --help failed'
fi

mkdir -p "$output"
[[ -d "$output" && ! -L "$output" ]] || fail 'output is not a directory'
output=$(cd "$output" && pwd -P)
name="epost-cli-v${version}-${target}"
archive="$output/$name.tar.gz"
checksum="$archive.sha256"
for destination in "$archive" "$checksum"; do
    [[ ! -e "$destination" && ! -L "$destination" ]] ||
        fail 'archive or checksum already exists; refusing to overwrite'
done

# Staging on the output filesystem allows no-clobber hard-link publication.
temporary=$(mktemp -d "$output/.epost-package.XXXXXXXX")
archive_published=0
complete=0
cleanup() {
    if [[ $archive_published -eq 1 && $complete -eq 0 ]]; then
        rm -f "$archive"
    fi
    rm -rf "$temporary"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir "$temporary/$name"
cp "$binary" "$temporary/$name/epost-cli"
cp "$binary" "$temporary/$name/epost"
cp "$repository/LICENSE" "$temporary/$name/LICENSE"
cp "$repository/README.md" "$temporary/$name/README.md"
chmod 0755 "$temporary/$name" "$temporary/$name/epost-cli" "$temporary/$name/epost"
chmod 0644 "$temporary/$name/LICENSE" "$temporary/$name/README.md"
# macOS tar otherwise may add AppleDouble entries for copied extended attributes.
COPYFILE_DISABLE=1 tar -czf "$temporary/$name.tar.gz" -C "$temporary" "$name"
(
    cd "$temporary"
    shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256"
)
chmod 0644 "$temporary/$name.tar.gz" "$temporary/$name.tar.gz.sha256"

# link refuses every existing destination, including directories and symlinks.
[[ ! -e "$archive" && ! -L "$archive" && ! -e "$checksum" && ! -L "$checksum" ]] ||
    fail 'archive or checksum appeared during packaging; refusing to overwrite'
link "$temporary/$name.tar.gz" "$archive"
archive_published=1
link "$temporary/$name.tar.gz.sha256" "$checksum"
complete=1
printf '%s\n%s\n' "$archive" "$checksum"
