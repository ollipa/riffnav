#!/usr/bin/env bash
# Checks that a release tag is safe to publish: it names the version in
# Cargo.toml, points at a commit on main, and is the next version after the
# latest release (no going back, no skipped numbers).
#
# Usage: check-release.sh <tag>
set -euo pipefail

fail() {
    echo "error: $*" >&2
    exit 1
}

tag=${1:?usage: check-release.sh <tag>}

[[ $tag =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] ||
    fail "tag $tag isn't of the form vMAJOR.MINOR.PATCH"

version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')
[[ $tag == "v$version" ]] ||
    fail "tag $tag doesn't match Cargo.toml version $version"

commit=$(git rev-parse "$tag^{commit}")
git rev-parse --verify --quiet origin/main >/dev/null ||
    fail "origin/main isn't fetched"
git merge-base --is-ancestor "$commit" origin/main ||
    fail "tag $tag points at $commit, which isn't on main"

previous=$(git tag --list 'v*' |
    grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' |
    grep -vxF "$tag" |
    sort -V |
    tail -n 1 || true)

if [[ -z $previous ]]; then
    echo "$tag is the first release"
    exit 0
fi

IFS=. read -r major minor patch <<<"${previous#v}"
allowed=(
    "v$major.$minor.$((patch + 1))"
    "v$major.$((minor + 1)).0"
    "v$((major + 1)).0.0"
)
for next in "${allowed[@]}"; do
    if [[ $tag == "$next" ]]; then
        echo "$tag follows $previous"
        exit 0
    fi
done
fail "tag $tag doesn't follow $previous; expected one of: ${allowed[*]}"
