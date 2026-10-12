#!/usr/bin/env bash
# Synthetic Git history only: no remote access or real repository writes.
set -euo pipefail
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
validator=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)/validate-ref.sh
temporary=$(mktemp -d /tmp/epost-release-ref-test.XXXXXXXX)
trap 'rm -rf "$temporary"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
git init --quiet "$temporary/repository"
cd "$temporary/repository"
git config user.name 'Synthetic Release Test'
git config user.email 'release-test@example.invalid'
git config commit.gpgsign false
git config tag.gpgsign false
git commit --quiet --allow-empty -m 'synthetic main'
main_commit=$(git rev-parse HEAD)
git update-ref refs/remotes/origin/main "$main_commit"
git tag -a v0.1.0 -m 'synthetic annotated tag'
bash "$validator" 0.1.0 tag v0.1.0 > "$temporary/accepted"
[[ $(sed -n 's/^commit=//p' "$temporary/accepted") == "$main_commit" ]]
[[ $(sed -n 's/^tag=//p' "$temporary/accepted") == v0.1.0 ]]
expect_failure() {
    if bash "$validator" "$@" > "$temporary/refused.stdout" 2> "$temporary/refused.stderr"; then
        printf 'An invalid release ref was accepted.\n' >&2
        exit 1
    fi
}
expect_failure 0.2.0 tag v0.1.0
expect_failure 0.1.0 tag v0.1.1
expect_failure 0.1.0 tag v0.1.0-rc.1
expect_failure '0.1.0/unsafe' tag v0.1.0
expect_failure 0.1.0 unexpected v0.1.0
git commit --quiet --allow-empty -m 'synthetic unmerged work'
expect_failure 0.1.0 tag v0.1.0
git tag v0.2.0
expect_failure 0.2.0 tag v0.2.0
# PR/manual builds may preview unmerged work, but do not authorize publication.
bash "$validator" 0.2.0 branch synthetic-preview > "$temporary/preview"
git update-ref refs/remotes/origin/main HEAD
bash "$validator" 0.2.0 tag v0.2.0 > "$temporary/merged"
printf 'Synthetic release-ref tests passed.\n'
