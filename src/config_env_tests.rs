//! Tests for the `${VAR}` env-resolution path in `Config::load`.

use secrecy::{ExposeSecret, SecretString};

use super::resolve_secret_env;

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn passthrough_value_is_unchanged() {
    let v = SecretString::from("literal-value".to_owned());
    let out = resolve_secret_env(&v, "test").expect("ok");
    assert_eq!(out.expose_secret(), "literal-value");
}

#[test]
#[expect(
    unsafe_code,
    reason = "set_var is unsafe in 2024 edition; test isolates the var"
)]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn env_substitution_resolves() {
    // SAFETY: test is single-threaded; we set a unique var name.
    unsafe {
        std::env::set_var("EPISTOLE_TEST_TOKEN_X1", "the-real-secret");
    }
    let v = SecretString::from("${EPISTOLE_TEST_TOKEN_X1}".to_owned());
    let out = resolve_secret_env(&v, "test").expect("ok");
    assert_eq!(out.expose_secret(), "the-real-secret");
    unsafe {
        std::env::remove_var("EPISTOLE_TEST_TOKEN_X1");
    }
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn unset_env_var_errors() {
    let v = SecretString::from("${EPISTOLE_TEST_DEFINITELY_UNSET}".to_owned());
    let err = resolve_secret_env(&v, "test").unwrap_err();
    assert!(
        format!("{err}").contains("EPISTOLE_TEST_DEFINITELY_UNSET"),
        "error mentions the missing var"
    );
}

#[test]
#[expect(
    unsafe_code,
    reason = "set_var is unsafe in 2024 edition; test isolates the var"
)]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn empty_env_var_errors() {
    unsafe {
        std::env::set_var("EPISTOLE_TEST_EMPTY", "");
    }
    let v = SecretString::from("${EPISTOLE_TEST_EMPTY}".to_owned());
    let err = resolve_secret_env(&v, "test").unwrap_err();
    assert!(format!("{err}").contains("empty"));
    unsafe {
        std::env::remove_var("EPISTOLE_TEST_EMPTY");
    }
}
