#!/usr/bin/env bash
set -euo pipefail

# WHY(#70): shared by release.yml and the collision test so the fixture
# proving append-once behavior exercises this exact invocation, not a
# reimplementation of it. No --clobber: gh release upload hard-fails when
# an asset of this name already exists on the tag. That absence IS the
# enforcement mechanism, not a guard wrapped around one.

if [[ $# -lt 2 ]]; then
  echo "usage: release-upload.sh <tag> <file>..." >&2
  exit 64
fi

tag="$1"
shift

gh release upload "$tag" "$@"
