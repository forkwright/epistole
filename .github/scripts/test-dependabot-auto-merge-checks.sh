#!/usr/bin/env bash
# Regression test for the "Wait for CI checks to pass" guard in
# dependabot-auto-merge.yml.
set -euo pipefail

# Extracts that step's literal `run: |` body from the SHIPPED workflow
# file (not a re-implementation of its logic) and runs it, with `gh`
# stubbed, against synthetic `gh pr checks` output covering every bucket
# the kanon-lint job can report.
#
# WHY this exists (epistole#111): the guard folded kanon-lint into a
# uniform `bucket == "pass"` loop. kanon-lint job-level `if`-skips itself
# whenever FLEET_REPO_TOKEN is absent from hosted runners (epistole#107,
# still open) -- true today and for the foreseeable future -- so its
# bucket is permanently "skipping", never "pass", and the guard refused
# every dependabot auto-merge unconditionally. Nothing executed this
# script's logic outside a live GitHub Actions run, so the defect shipped
# unnoticed. This test runs on every push (see ci.yml's
# `test-auto-merge-guard` job) so the same shape fails here first.

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && cd .. && pwd)"
readonly WORKFLOW_FILE="${REPO_ROOT}/.github/workflows/dependabot-auto-merge.yml"
readonly STEP_NAME="Wait for CI checks to pass"

# Dependency-free literal-block-scalar extractor (awk + bash only -- no
# yq/python-yaml required, so this runs unmodified on any hosted runner).
extract_run_block() {
  local file="$1" step_name="$2"
  awk -v step="- name: ${step_name}" '
    BEGIN { in_step = 0; in_run = 0; base_indent = -1 }
    {
      line = $0
      if (!in_run && index(line, step) > 0) { in_step = 1; next }
      if (in_step && !in_run) {
        if (line ~ /^[[:space:]]*run: \|[[:space:]]*$/) {
          in_run = 1
          match(line, /^[[:space:]]*/)
          base_indent = RLENGTH
          next
        }
        if (line ~ /^      - name:/) { in_step = 0 }
        next
      }
      if (in_run) {
        if (line ~ /^[[:space:]]*$/) { print ""; next }
        match(line, /^[[:space:]]*/)
        indent = RLENGTH
        if (indent <= base_indent) { exit }
        print substr(line, base_indent + 3)
      }
    }
  ' "$file"
}

readonly WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

readonly GUARD_SCRIPT="${WORK_DIR}/guard.sh"
extract_run_block "$WORKFLOW_FILE" "$STEP_NAME" > "$GUARD_SCRIPT"

if [[ ! -s "$GUARD_SCRIPT" ]]; then
  echo "test-dependabot-auto-merge-checks: extracted an empty script for" \
    "step '${STEP_NAME}' from ${WORKFLOW_FILE} -- the step was likely" \
    "renamed or restructured; update STEP_NAME or the extractor." >&2
  exit 1
fi

readonly STUB_BIN_DIR="${WORK_DIR}/bin"
mkdir -p "$STUB_BIN_DIR"
ln -s "${SCRIPT_DIR}/gh-stub" "${STUB_BIN_DIR}/gh"

build_checks() {
  local kanon_lint_bucket="$1"
  jq -n --arg klb "$kanon_lint_bucket" '
    [
      {name:"Format", bucket:"pass"},
      {name:"Check", bucket:"pass"},
      {name:"Clippy", bucket:"pass"},
      {name:"Test", bucket:"pass"},
      {name:"kanon-lint", bucket:$klb},
      {name:"cargo deny", bucket:"pass"},
      {name:"cargo audit", bucket:"pass"}
    ]'
}

run_guard() {
  local checks_json="$1"
  local exit_code=0
  CHECKS_JSON="$checks_json" \
    PATH="${STUB_BIN_DIR}:${PATH}" \
    PR_URL="https://github.com/forkwright/epistole/pull/0" \
    GH_TOKEN="stub" \
    bash "$GUARD_SCRIPT" >/dev/null 2>&1 || exit_code=$?
  echo "$exit_code"
}

# WHY return, not a shared counter mutated from inside the function: a
# function assigning a variable it did not declare `local` is exactly what
# SHELL/missing-local exists to catch (accidental global leakage) --
# reporting failure via exit status and tallying at the call site keeps
# every function-local variable actually local.
check_case() {
  local label="$1" kanon_lint_bucket="$2" expect_exit="$3"
  local checks_json actual_exit
  checks_json="$(build_checks "$kanon_lint_bucket")"
  actual_exit="$(run_guard "$checks_json")"
  if [[ "$actual_exit" != "$expect_exit" ]]; then
    echo "FAIL: $label -- kanon-lint bucket='${kanon_lint_bucket}':" \
      "expected exit ${expect_exit}, got ${actual_exit}" >&2
    return 1
  fi
  echo "ok: $label (kanon-lint=${kanon_lint_bucket} -> exit ${actual_exit})"
}

failures=0

# kanon-lint SKIPPED (FLEET_REPO_TOKEN absent, epistole#107/#109's live
# state) must NOT block auto-merge -- this is the epistole#111 regression.
check_case "kanon-lint skipping is treated as acceptable" "skipping" 0 \
  || failures=$((failures + 1))

# kanon-lint genuinely running and finding something MUST still block.
check_case "kanon-lint fail still refuses auto-merge" "fail" 1 \
  || failures=$((failures + 1))

# kanon-lint passing outright (FLEET_REPO_TOKEN provisioned some day) is
# unaffected.
check_case "kanon-lint pass is unaffected" "pass" 0 \
  || failures=$((failures + 1))

if (( failures > 0 )); then
  echo "test-dependabot-auto-merge-checks: ${failures} case(s) failed" >&2
  exit 1
fi

echo "test-dependabot-auto-merge-checks: all cases passed"
