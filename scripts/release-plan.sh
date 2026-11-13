#!/usr/bin/env bash
# Decides what a run of the release workflow does, from the ref that started it.
# Prints `version=`, `release=true|false` and `dry_run=true|false` (lines for
# $GITHUB_OUTPUT).
#
#   scripts/release-plan.sh main              # a push to main: release the version in
#                                             # crates/app/Cargo.toml unless it already is
#   scripts/release-plan.sh v0.2.2-dryrun.1   # trial run: builds everything, publishes nothing
#
# Checking for the release needs `gh` signed in (GH_TOKEN in Actions) and
# GITHUB_REPOSITORY, or a checkout gh can find the repository from.
set -euo pipefail
cd "$(dirname "$0")/.."

ref=${1:?usage: scripts/release-plan.sh <main | tag>}
version=$(grep -m1 '^version' crates/app/Cargo.toml | cut -d'"' -f2)

if [[ "$ref" == main ]]; then
  dry_run=false
  if gh release view "v$version" ${GITHUB_REPOSITORY:+--repo "$GITHUB_REPOSITORY"} >/dev/null 2>&1; then
    echo "v$version is already released; nothing to do. Bump version in crates/app/Cargo.toml to release." >&2
    release=false
  else
    release=true
  fi
elif [[ "$ref" =~ ^v${version//./\\.}-dryrun\.[0-9]+$ ]]; then
  dry_run=true
  release=true
else
  echo "Tag $ref is not a trial run of version $version in crates/app/Cargo.toml (want v$version-dryrun.<n>)." >&2
  exit 1
fi

echo "version=$version"
echo "release=$release"
echo "dry_run=$dry_run"
