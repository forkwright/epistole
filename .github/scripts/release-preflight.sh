#!/usr/bin/env bash
set -euo pipefail

# WHY(#70): a release that already carries a published asset means
# release.yml has already run for this tag. A further run is a republish
# attempt, not a retry -- corrections ship as a new tag (append-once), so
# this refuses before the build matrix spends any compute on a run that
# can only end in a collision.

if [[ $# -ne 1 ]]; then
  echo "usage: release-preflight.sh <tag>" >&2
  exit 64
fi

tag="$1"

if ! view="$(gh release view "$tag" --json assets 2>&1)"; then
  if [[ "$view" == *"release not found"* ]]; then
    echo "no release found for $tag yet -- clear to publish"
    exit 0
  fi
  echo "::error::could not check existing assets for $tag: $view" >&2
  exit 1
fi

existing="$(jq '.assets | length' <<<"$view")"

if [[ "$existing" -gt 0 ]]; then
  echo "::error::release $tag already has $existing published asset(s) -- refusing to republish. Cut a new version/tag instead." >&2
  exit 1
fi

echo "no existing assets on $tag -- clear to publish"
