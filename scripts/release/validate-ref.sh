#!/usr/bin/env bash
# Read-only validation of the checked-out commit before release builds.
set -euo pipefail

fail() { printf 'release: %s\n' "$1" >&2; exit 1; }
[[ $# -eq 3 ]] || fail 'usage: validate-ref.sh VERSION REF_TYPE REF_NAME'
version=$1
ref_type=$2
ref_name=$3
# Cargo supplies a valid SemVer; this bound also keeps outputs and Git refs safe.
[[ ${#version} -le 128 && $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$ ]] || fail 'invalid package version'
[[ "$ref_type" == tag || "$ref_type" == branch ]] || fail 'unexpected ref type'
tag="v$version"
commit=$(git rev-parse --verify HEAD)
if [[ "$ref_type" == tag ]]; then
    [[ "$ref_name" == "$tag" ]] || fail 'tag must exactly match the package version'
    tag_commit=$(git rev-parse --verify "refs/tags/$tag^{commit}" 2>/dev/null) || fail 'release tag is missing'
    [[ "$tag_commit" == "$commit" ]] || fail 'tag commit differs from checkout'
    git merge-base --is-ancestor "$commit" origin/main || fail 'release commit must be merged into main'
fi
printf 'version=%s\ntag=%s\ncommit=%s\n' "$version" "$tag" "$commit"
