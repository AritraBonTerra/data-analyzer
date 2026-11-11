#!/usr/bin/env bash
# Checks a pushed tag against the app version before a release is built.
# Prints `version=<version>` and `dry_run=true|false` (lines for $GITHUB_OUTPUT).
#
#   scripts/release-tag.sh v0.2.0            # release: tag must be v + the version
#   scripts/release-tag.sh v0.2.0-dryrun.1   # trial run: builds everything, publishes nothing
set -euo pipefail
cd "$(dirname "$0")/.."

tag=${1:?usage: scripts/release-tag.sh <tag>}
version=$(grep -m1 '^version' crates/app/Cargo.toml | cut -d'"' -f2)

if [[ "$tag" == "v$version" ]]; then
  dry_run=false
elif [[ "$tag" =~ ^v${version//./\\.}-dryrun\.[0-9]+$ ]]; then
  dry_run=true
else
  echo "Tag $tag does not match version $version in crates/app/Cargo.toml (want v$version or v$version-dryrun.<n>)." >&2
  exit 1
fi

echo "version=$version"
echo "dry_run=$dry_run"
