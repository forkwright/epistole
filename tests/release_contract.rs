//! Release-pipeline contract tests (forkwright/epistole#70).
//!
//! Immutability (a published asset never changes underneath a tag) and
//! reproducibility (the same inputs produce the same build) are two
//! separable properties -- this file checks both, and each assertion
//! names which one it guards. Following the `deploy_contract.rs`
//! pattern: the shipped workflow/script/config files are embedded with
//! `include_str!` so a regression breaks the build rather than skipping
//! the check at run time.

const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
const RELEASE_UPLOAD_SCRIPT: &str = include_str!("../.github/scripts/release-upload.sh");
const RELEASE_PREFLIGHT_SCRIPT: &str = include_str!("../.github/scripts/release-preflight.sh");
const RUST_TOOLCHAIN: &str = include_str!("../rust-toolchain.toml");
const CROSS_CONFIG: &str = include_str!("../Cross.toml");

// --- Immutability: a published asset never changes underneath a tag. ---

#[test]
fn release_upload_never_clobbers() {
    assert!(
        !RELEASE_WORKFLOW.contains("--clobber"),
        "release.yml must not pass --clobber to gh release upload -- that \
         flag is exactly what lets a rerun silently replace a published \
         asset's bytes"
    );
    assert!(
        !RELEASE_UPLOAD_SCRIPT.contains("--clobber"),
        "the shared upload script must not pass --clobber either -- \
         release.yml calls this script, so a --clobber added here would \
         reopen the hole even with the workflow file clean"
    );
}

#[test]
fn release_workflow_has_exactly_one_publish_path() {
    // WHY: append-once is only a real control if publishing has exactly
    // one code path. A second inline `gh release upload`, or a
    // third-party upload action, would bypass release-upload.sh's
    // no-clobber invariant entirely.
    assert!(
        RELEASE_WORKFLOW.contains("release-upload.sh"),
        "release.yml must publish through .github/scripts/release-upload.sh"
    );
    assert!(
        !RELEASE_WORKFLOW.contains("gh release upload"),
        "found an inline `gh release upload` in release.yml -- publication \
         must go through release-upload.sh so the no-clobber guard cannot \
         be bypassed by a second call site"
    );
    assert!(
        // WHY "actions/upload-release-asset" not the bare phrase: the
        // anchore/sbom-action step below sets `upload-release-assets:
        // false`, whose key is a substring of the bare phrase and would
        // false-positive this check on a workflow that has no such action.
        !RELEASE_WORKFLOW.contains("actions/upload-release-asset"),
        "found a third-party release-asset upload action -- another \
         publish path bypasses release-upload.sh's no-clobber guard"
    );
}

#[test]
#[expect(clippy::expect_used, reason = "a failed setup step is a test failure")]
fn release_workflow_preflights_before_the_build_matrix() {
    // WHY: append-once must refuse BEFORE the build job spends compute,
    // not only when the upload step is finally reached -- a
    // build-then-refuse still burns the exact runs a rerun should never
    // start doing.
    assert!(
        RELEASE_WORKFLOW.contains("release-preflight.sh"),
        "release.yml must run release-preflight.sh to refuse a republish \
         before the build matrix starts"
    );

    let preflight_pos = RELEASE_WORKFLOW
        .find("\n  preflight:")
        .expect("release.yml must define a preflight job");
    let build_pos = RELEASE_WORKFLOW
        .find("\n  build:")
        .expect("release.yml must define a build job");
    assert!(
        preflight_pos < build_pos,
        "the preflight job must be defined before the build job in release.yml"
    );

    let build_block = &RELEASE_WORKFLOW[build_pos..];
    let needs_line = build_block
        .lines()
        .find(|l| l.trim_start().starts_with("needs:"))
        .expect("build job must declare `needs:`");
    assert!(
        needs_line.contains("preflight"),
        "build job's `needs:` must include preflight, or a rerun of an \
         already-published tag still builds before the collision is \
         caught: {needs_line}"
    );
}

#[test]
#[expect(clippy::expect_used, reason = "a failed setup step is a test failure")]
fn release_preflight_hard_fails_on_an_existing_asset() {
    assert!(
        RELEASE_PREFLIGHT_SCRIPT.contains("gh release view"),
        "preflight must query the tag's existing release state"
    );
    assert!(
        RELEASE_PREFLIGHT_SCRIPT.contains("-gt 0"),
        "preflight must branch on whether any asset already exists, not \
         merely record a count"
    );

    let collision_branch = RELEASE_PREFLIGHT_SCRIPT
        .split("-gt 0")
        .nth(1)
        .expect("a `-gt 0` comparison must be followed by its branch body");
    assert!(
        collision_branch.contains("exit 1"),
        "the existing-asset branch must hard-fail (non-zero exit), not \
         merely warn: {collision_branch}"
    );
}

// --- Reproducibility: build inputs are pinned, not floating. ---

#[test]
fn cargo_build_and_test_are_locked_to_the_committed_lockfile() {
    assert!(
        RELEASE_WORKFLOW.contains("cargo test --workspace --locked"),
        "the test job must fail on Cargo.lock drift, not silently update it"
    );
    assert!(
        RELEASE_WORKFLOW.contains("cargo build --release --locked --target"),
        "the native build must be --locked"
    );
    assert!(
        RELEASE_WORKFLOW.contains("cross build --release --locked --target"),
        "the cross build must be --locked"
    );
}

#[test]
fn cross_tool_version_is_pinned_exactly() {
    assert!(
        RELEASE_WORKFLOW.contains("cargo install cross --version"),
        "cross must be installed at an exact --version, not whatever \
         `cargo install cross` resolves to on the day the job runs"
    );
    assert!(
        !RELEASE_WORKFLOW.contains("cargo install cross --locked"),
        "found an unversioned `cargo install cross` -- --locked alone \
         only pins cross's own dependency graph, not which cross release \
         gets installed"
    );
}

#[test]
#[expect(clippy::expect_used, reason = "a failed setup step is a test failure")]
fn rust_toolchain_channel_is_an_exact_version_not_a_floating_track() {
    let channel = RUST_TOOLCHAIN
        .lines()
        .find(|l| l.trim_start().starts_with("channel"))
        .and_then(|l| l.split('"').nth(1))
        .expect("rust-toolchain.toml must declare a channel string");

    assert!(
        !["stable", "beta", "nightly"].contains(&channel),
        "rust-toolchain.toml channel = \"{channel}\" floats to whatever \
         that track resolves to on the day CI runs -- pin an exact \
         MAJOR.MINOR.PATCH release instead"
    );
    assert!(
        channel.split('.').count() == 3
            && channel
                .split('.')
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
        "channel = \"{channel}\" is not a MAJOR.MINOR.PATCH version"
    );
}

#[test]
#[expect(clippy::expect_used, reason = "a failed setup step is a test failure")]
fn cross_build_container_is_pinned_to_a_content_digest() {
    assert!(
        CROSS_CONFIG.contains("aarch64-unknown-linux-gnu"),
        "Cross.toml must configure the cross-compiled target"
    );

    let image_line = CROSS_CONFIG
        .lines()
        .find(|l| l.trim_start().starts_with("image"))
        .expect("Cross.toml must set an `image` for the cross-compiled target");

    let digest = image_line
        .split("@sha256:")
        .nth(1)
        .map(|d| d.trim_matches(|c: char| !c.is_ascii_hexdigit()))
        .unwrap_or_default();

    assert_eq!(
        digest.len(),
        64,
        "image = \"{image_line}\" is not pinned to a full sha256 content \
         digest (64 hex chars) -- a tag alone can be repointed at the \
         registry without this reference changing"
    );
    assert!(
        digest.chars().all(|c| c.is_ascii_hexdigit()),
        "digest suffix on {image_line} is not hex"
    );
}
