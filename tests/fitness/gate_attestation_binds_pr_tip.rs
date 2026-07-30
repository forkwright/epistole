//! Fitness function: the gate-attestation trailer check reads the PR tip only.
//!
//! WHY: the original check walked `origin/<base>..HEAD` and passed as soon as
//! any commit in the range carried a `Gate-Passed:` trailer. A branch could
//! therefore be stamped once, take further unstamped commits, and still show a
//! green gate — the tree that actually merged was never gated. kanon#2399 fixed
//! this in the canonical `forkwright/.github` workflows by binding the check to
//! `github.event.pull_request.head.sha`; this repo carries its own copy of that
//! workflow, so it needs its own guard against the range form returning.
//!
//! INVARIANT: the trailer-verify step resolves the commit it inspects from the
//! PR head SHA, and never from a commit range.
//!
//! WARNING: this reads the workflow as text. It cannot tell whether GitHub
//! actually populates `head.sha`, nor whether the step's `if:` condition lets it
//! run at all — it only establishes which commit the script names. The waiver
//! condition is a separate concern and is deliberately not asserted here.
//!
//! NOTE: measured both ways before being relied on. Against the pre-fix file
//! (`commits=$(git log --format="%H" "origin/${{ github.base_ref }}..HEAD")`)
//! `range_forms` is non-empty and this test fails; against the fixed file it
//! passes.

use std::path::PathBuf;

/// Shell and expression fragments that reach for a commit *range* rather than a
/// single commit. Any of these in the trailer-verify step means the check can be
/// satisfied by an ancestor.
const RANGE_FORMS: &[&str] = &["..HEAD", "github.base_ref", "for sha in"];

fn workflow_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/gate-attestation.yml")
}

#[test]
fn gate_attestation_trailer_check_is_bound_to_the_pr_tip() {
    let path = workflow_path();
    let workflow = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    // INVARIANT: the guard is only meaningful while the step it guards exists.
    // If the workflow is ever replaced by a `uses:` delegation to the canonical
    // fleet workflow, this test must be deleted rather than left passing
    // vacuously over a file that no longer performs the check.
    assert!(
        workflow.contains("Verify Gate-Passed trailer"),
        "{} no longer has a 'Verify Gate-Passed trailer' step — this fitness \
         function measures nothing and must be updated or removed",
        path.display()
    );

    assert!(
        workflow.contains("github.event.pull_request.head.sha"),
        "{} must resolve the commit it checks from the PR head SHA so the tree \
         that merges is the tree that was gated (kanon#2399)",
        path.display()
    );

    let range_forms: Vec<&str> = RANGE_FORMS
        .iter()
        .copied()
        .filter(|form| workflow.contains(form))
        .collect();

    assert!(
        range_forms.is_empty(),
        "{} reaches for a commit range ({}) in the gate-attestation check. The \
         range form passes when any ancestor carries a Gate-Passed trailer, so \
         an unstamped tip rides an earlier commit's attestation and the merged \
         tree is never gated. Bind the check to \
         github.event.pull_request.head.sha instead. See kanon#2399.",
        path.display(),
        range_forms.join(", ")
    );
}
