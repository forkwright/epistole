//! Release-pipeline contract tests (forkwright/epistole#70).
//!
//! WHY phase 1 only: this is the negative-fixture step -- these two
//! assertions are checked against files that already exist on main, so
//! they can be watched failing before the rest of the fix (new
//! Cross.toml / .github/scripts files, and the remaining release.yml
//! changes) lands. The full contract is added in the next commit.

const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
const RUST_TOOLCHAIN: &str = include_str!("../rust-toolchain.toml");

#[test]
fn release_upload_never_clobbers() {
    assert!(
        !RELEASE_WORKFLOW.contains("--clobber"),
        "release.yml must not pass --clobber to gh release upload -- that \
         flag is exactly what lets a rerun silently replace a published \
         asset's bytes"
    );
}

#[test]
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
}
