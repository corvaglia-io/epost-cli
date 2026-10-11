#!/usr/bin/env bash
# Entirely synthetic packaging tests. No provider API or developer binary runs.
set -euo pipefail
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
export LC_ALL=C

fail() {
    printf 'test-package: %s\n' "$1" >&2
    exit 1
}

repository=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
package="$repository/scripts/release/package.sh"
temporary=$(mktemp -d /tmp/epost-package-test.XXXXXXXX)
trap 'rm -rf "$temporary"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
fixture="$temporary/synthetic binary"

make_fixture() {
    # Only literal test versions are passed here, never runtime provider data.
    cat > "$fixture" <<'FIXTURE'
#!/bin/sh
test -z "${HOME+x}" || exit 91
test -z "${EPOST_USERNAME+x}" || exit 92
test -z "${EPOST_PASSWORD+x}" || exit 93
test -z "${EPOST_BEARER_TOKEN+x}" || exit 94
test -z "${EPOST_API_KEY+x}" || exit 95
test -z "${GITHUB_TOKEN+x}" || exit 96
test -z "${TEST_CREDENTIAL+x}" || exit 97
case "$1" in
    --version)
FIXTURE
    printf "        printf 'epost-cli %s\\n'\n" "$1" >> "$fixture"
    cat >> "$fixture" <<'FIXTURE'
        ;;
    --help) printf 'Synthetic offline help\n' ;;
    *) exit 98 ;;
esac
FIXTURE
    chmod 0755 "$fixture"
}

expect_failure() {
    if bash "$package" "$@" > "$temporary/refusal.stdout" 2> "$temporary/refusal.stderr"; then
        fail 'an invalid packaging invocation unexpectedly succeeded'
    fi
}

mode_of() {
    if stat -c '%a' "$1" 2>/dev/null; then
        return
    fi
    stat -f '%Lp' "$1"
}

make_fixture 0.1.0
target=x86_64-unknown-linux-gnu
name="epost-cli-v0.1.0-$target"
output="$temporary/output with spaces"
EPOST_USERNAME=synthetic@example.invalid EPOST_PASSWORD=synthetic-password \
EPOST_BEARER_TOKEN=synthetic-token EPOST_API_KEY=synthetic-key \
GITHUB_TOKEN=synthetic-github-token TEST_CREDENTIAL=synthetic-credential \
    bash "$package" 0.1.0 "$target" "$fixture" "$output" > "$temporary/success.stdout"

archive="$output/$name.tar.gz"
checksum="$archive.sha256"
[[ -f "$archive" && -f "$checksum" ]] || fail 'release files are missing'
tar -tzf "$archive" | sort > "$temporary/actual-contents"
printf '%s\n' "$name/" "$name/LICENSE" "$name/README.md" "$name/epost-cli" |
    sort > "$temporary/expected-contents"
cmp "$temporary/expected-contents" "$temporary/actual-contents" ||
    fail 'archive contains unexpected entries'
mkdir "$temporary/extracted"
tar -xzf "$archive" -C "$temporary/extracted"
[[ $(mode_of "$temporary/extracted/$name/epost-cli") == 755 ]] ||
    fail 'packaged binary mode is not 0755'
[[ $(mode_of "$temporary/extracted/$name/LICENSE") == 644 ]] ||
    fail 'packaged LICENSE mode is not 0644'
[[ $(mode_of "$temporary/extracted/$name/README.md") == 644 ]] ||
    fail 'packaged README mode is not 0644'
cmp "$fixture" "$temporary/extracted/$name/epost-cli" || fail 'binary changed'
cmp "$repository/LICENSE" "$temporary/extracted/$name/LICENSE" || fail 'LICENSE changed'
cmp "$repository/README.md" "$temporary/extracted/$name/README.md" || fail 'README changed'
read -r recorded_hash recorded_name < "$checksum"
[[ $recorded_hash =~ ^[0-9a-f]{64}$ && "$recorded_name" == "$name.tar.gz" ]] ||
    fail 'checksum does not use the relative archive filename'
(
    cd "$output"
    shasum -a 256 -c "$name.tar.gz.sha256" > "$temporary/checksum-verification"
)

# Refusals must preserve already published bytes and leave no staging directories.
expect_failure 0.1.0 "$target" "$fixture" "$output"
read -r unchanged_hash unchanged_name < "$checksum"
[[ "$unchanged_hash" == "$recorded_hash" && "$unchanged_name" == "$recorded_name" ]] ||
    fail 'existing checksum was overwritten'
(
    cd "$output"
    shasum -a 256 -c "$name.tar.gz.sha256" >/dev/null
)
for version in v0.1.0 01.1.0 0.01.0 0.1.00 0.1 0.1.0-01 '0.1.0/unsafe' '0.1.0+bad/segment' '0.1.0-'; do
    expect_failure "$version" "$target" "$fixture" "$temporary/refused"
done
expect_failure 0.1.0 unsupported-target "$fixture" "$temporary/refused"
expect_failure
expect_failure 0.1.0 "$target" "$temporary/missing" "$temporary/refused"
expect_failure 0.1.0 "$target" "$temporary" "$temporary/refused"
ln -s "$fixture" "$temporary/symlink-binary"
expect_failure 0.1.0 "$target" "$temporary/symlink-binary" "$temporary/refused"
chmod 0644 "$fixture"
expect_failure 0.1.0 "$target" "$fixture" "$temporary/refused"
chmod 0755 "$fixture"
expect_failure 0.2.0 "$target" "$fixture" "$temporary/refused"

mkdir "$temporary/checksum-only"
printf 'synthetic existing checksum\n' > "$temporary/checksum-only/$name.tar.gz.sha256"
expect_failure 0.1.0 "$target" "$fixture" "$temporary/checksum-only"
[[ ! -e "$temporary/checksum-only/$name.tar.gz" ]] ||
    fail 'archive was published despite an existing checksum'
[[ $(cat "$temporary/checksum-only/$name.tar.gz.sha256") == 'synthetic existing checksum' ]] ||
    fail 'existing standalone checksum was overwritten'
mkdir "$temporary/archive-only"
printf 'synthetic existing archive\n' > "$temporary/archive-only/$name.tar.gz"
expect_failure 0.1.0 "$target" "$fixture" "$temporary/archive-only"
[[ ! -e "$temporary/archive-only/$name.tar.gz.sha256" ]] ||
    fail 'checksum was published despite an existing archive'
[[ $(cat "$temporary/archive-only/$name.tar.gz") == 'synthetic existing archive' ]] ||
    fail 'existing standalone archive was overwritten'
mkdir "$temporary/archive-directory"
mkdir "$temporary/archive-directory/$name.tar.gz"
expect_failure 0.1.0 "$target" "$fixture" "$temporary/archive-directory"
mkdir "$temporary/archive-symlink"
ln -s "$temporary/missing" "$temporary/archive-symlink/$name.tar.gz"
expect_failure 0.1.0 "$target" "$fixture" "$temporary/archive-symlink"

make_fixture 0.1.0
printf '\nexit 99\n' >> "$fixture"
expect_failure 0.1.0 "$target" "$fixture" "$temporary/refused"
make_fixture 0.1.0
# Keep --version successful while checking that a failed --help aborts packaging.
sed 's/--help) printf/--help) exit 99; printf/' "$fixture" > "$temporary/failed-help"
chmod 0755 "$temporary/failed-help"
expect_failure 0.1.0 "$target" "$temporary/failed-help" "$temporary/refused"

# All target names and a nontrivial valid SemVer remain accepted.
for target in aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
    bash "$package" 0.1.0 "$target" "$fixture" "$temporary/$target" >/dev/null
done
make_fixture 0.1.0-rc.1+build.007
bash "$package" 0.1.0-rc.1+build.007 x86_64-unknown-linux-gnu "$fixture" "$temporary/prerelease" >/dev/null

shopt -s nullglob
leftovers=("$temporary"/*/.epost-package.*)
[[ ${#leftovers[@]} -eq 0 ]] || fail 'packaging left staging directories behind'
printf 'Synthetic packaging tests passed.\n'
