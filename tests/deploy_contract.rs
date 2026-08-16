//! Deployment-contract tests.
//!
//! The deploy bundle is four artifacts that must agree with one another:
//! `epistole.example.toml`, `deploy/epistole.service`,
//! `deploy/Caddyfile.snippet`, and `DEPLOY.md`. Each can drift on its own,
//! and drift is only visible to an operator halfway through a deploy. These
//! tests pin the agreements the runbook asks an operator to rely on.
//!
//! WHY: the artifacts are embedded with `include_str!` so a rename breaks the
//! build rather than skipping the check at run time.

use epistole::Config;

const EXAMPLE_TOML: &str = include_str!("../epistole.example.toml");
const CADDY_SNIPPET: &str = include_str!("../deploy/Caddyfile.snippet");
const SERVICE_UNIT: &str = include_str!("../deploy/epistole.service");
const RUNBOOK: &str = include_str!("../DEPLOY.md");

/// Secrets that clear the strength floors and miss every blocked pattern.
const TOKEN_SECRET: &str = "Zx7Qv2Lm9Kd4Rt8Wn1Yb6Hf3Jc5Pg0Su";
const SEND_AUTH_TOKEN: &str = "Vb3Nm8Qw1Ei6Rt9Yu2Io5Pa7";
const WEBHOOK_AUTH_TOKEN: &str = "Ce4Ht9Ok2Sw5Xz8Bd1Fg6Ju3";
const SMTP_PASSWORD: &str = "Kd9Fh2Lq7Zx4Cv1Bn6Mw3Jt8";

/// The shipped example with its placeholders filled in, as step 5 of the
/// runbook instructs.
fn filled_example() -> String {
    EXAMPLE_TOML
        .replace("REPLACE_WITH_BASE64_RANDOM_32", TOKEN_SECRET)
        .replace("REPLACE_WITH_BASE64_RANDOM_24_WEBHOOK", WEBHOOK_AUTH_TOKEN)
        .replace("REPLACE_WITH_BASE64_RANDOM_24", SEND_AUTH_TOKEN)
        .replace("REPLACE_WITH_POSTMARK_TOKEN", SMTP_PASSWORD)
}

/// The bind port the bundle is expected to agree on, read from the example
/// config so that file stays the single source. Empty when unparseable,
/// which fails the assertions below rather than the helper.
fn documented_port() -> String {
    EXAMPLE_TOML
        .lines()
        .find(|l| l.trim_start().starts_with("bind ="))
        .and_then(|l| l.rsplit(':').next())
        .map(|p| p.trim_matches(|c: char| !c.is_ascii_digit()).to_owned())
        .unwrap_or_default()
}

/// The single `nginx`-fenced code block in `DEPLOY.md` — the literal
/// text an operator pastes into NPM's per-host Advanced tab. Scoping to
/// just this block (rather than the whole runbook) matters for
/// `npm_advanced_tab_config_never_declares_log_format`: prose elsewhere
/// in the doc is free to describe the http-scope alternative without
/// tripping a check that exists to catch that directive landing where
/// NPM would reject it.
#[expect(
    clippy::expect_used,
    reason = "a malformed fixture is a test bug, not a runtime path"
)]
fn npm_advanced_tab_config() -> &'static str {
    let start = RUNBOOK
        .find("```nginx")
        .expect("DEPLOY.md must have an nginx code fence")
        + "```nginx".len();
    let rest = &RUNBOOK[start..];
    let end = rest.find("\n```").expect("the nginx code fence must close");
    &rest[..end]
}

/// The body of an exact-match `location = <path> { ... }` block inside
/// `text`, brace-balanced so a nested `{`/`}` pair can't truncate it
/// early. `None` if the block is absent or its closing brace is
/// missing — both are structural failures the caller should fail on,
/// not a `contains()` that would pass on the path string appearing
/// anywhere at all (a comment mentioning it, or a DIFFERENT location's
/// body) rather than inside that route's own block.
fn location_block<'a>(text: &'a str, path: &str) -> Option<&'a str> {
    let needle = format!("location = {path} {{");
    let start = text.find(&needle)? + needle.len();
    let mut depth = 1i32;
    for (offset, ch) in text[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

#[test]
#[expect(clippy::expect_used, reason = "a failed setup step is a test failure")]
fn example_config_loads_once_its_placeholders_are_filled_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("epistole.toml");
    std::fs::write(&path, filled_example()).expect("write config");

    let cfg = Config::load(&path).expect("filled example config must load");

    assert_eq!(cfg.bind, format!("127.0.0.1:{}", documented_port()));
}

#[test]
#[expect(clippy::expect_used, reason = "a failed setup step is a test failure")]
fn example_config_is_refused_for_its_secrets_not_its_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("epistole.toml");
    std::fs::write(&path, EXAMPLE_TOML).expect("write config");

    let err = Config::load(&path)
        .expect_err("the shipped example must not boot with placeholder secrets")
        .to_string();

    assert!(
        err.contains("token_secret"),
        "expected a secret-strength refusal naming the field, got: {err}"
    );
    assert!(
        !err.contains("unknown field") && !err.contains("parse"),
        "the example must be structurally valid; got a shape error: {err}"
    );
}

#[test]
fn deploy_artifacts_agree_on_the_bind_port() {
    let port = documented_port();
    assert!(!port.is_empty(), "example config must declare a bind port");

    for line in CADDY_SNIPPET
        .lines()
        .filter(|l| l.contains("reverse_proxy"))
    {
        assert!(
            line.contains(&format!("127.0.0.1:{port}")),
            "Caddy snippet proxies to a different port than the example config: {line}"
        );
    }

    let forward = RUNBOOK
        .lines()
        .find(|l| l.contains("Forward Port"))
        .unwrap_or_default();
    assert!(
        forward.contains(&port),
        "runbook forward port disagrees with the example config: {forward}"
    );
}

#[test]
fn caddy_site_label_does_not_append_a_second_tld() {
    assert!(
        !CADDY_SNIPPET.contains("<consumer-domain>."),
        "the placeholder is an apex domain, so appending a TLD yields example.com.com"
    );
}

#[test]
fn caddy_access_log_redacts_the_token_query_parameter() {
    // forkwright/epistole#66: /confirm, /unsubscribe, and
    // /unsubscribe/one-click carry a signed capability token (and,
    // nested inside it, the subscriber's email) as a `token` query
    // parameter. Caddy's default `format json` records the complete
    // request URI verbatim, so a deploy that reverted to it would
    // persist every one of those tokens to
    // /var/log/caddy/letters-access.log. Proxy-style companion to the
    // application-level capture tests in tests/tracing_redaction.rs.
    assert!(
        CADDY_SNIPPET.contains("format filter"),
        "access log must use Caddy's filter encoder, not plain `format json`, \
         to redact the token query parameter"
    );
    assert!(
        CADDY_SNIPPET.contains("request>uri query") && CADDY_SNIPPET.contains("delete token"),
        "the uri's `token` query parameter must be deleted before the request \
         reaches the access log"
    );
}

#[test]
fn npm_token_routes_redact_access_log_and_agree_on_the_forward_port() {
    // forkwright/epistole#104: unlike Caddy's per-site config (#66,
    // tested above), NPM's per-host Advanced tab cannot declare a
    // redacting log format — that's an http-context directive, one
    // level above what this tab reaches. The reachable mitigation is
    // `access_log off` on the three routes that carry the token query
    // parameter (GET /confirm, GET /unsubscribe, POST
    // /unsubscribe/one-click), each with its OWN proxy_pass since an
    // exact-match `location =` does not fall back to NPM's
    // auto-generated `location /`.
    //
    // This inspects the RENDERED config, not a flat substring search:
    // `location_block` isolates each route's own brace-balanced body
    // before either assertion runs, so a location that never redacts
    // (or a different route's body happening to contain the words
    // elsewhere) cannot pass by coincidence the way a whole-file
    // `.contains()` could.
    let port = documented_port();
    assert!(!port.is_empty(), "example config must declare a bind port");
    let expected_proxy_pass = format!("proxy_pass http://127.0.0.1:{port};");

    for path in ["/confirm", "/unsubscribe", "/unsubscribe/one-click"] {
        let block = location_block(npm_advanced_tab_config(), path).unwrap_or_else(|| {
            panic!(
                "DEPLOY.md's Advanced-tab Nginx config must define an exact-match \
                 `location = {path} {{ ... }}` block so this route's token query \
                 parameter is excluded from the NPM access log"
            )
        });
        assert!(
            block.contains("access_log off;"),
            "location = {path} block must turn off access logging — its query \
             string (the token) is otherwise recorded verbatim to the NPM \
             access log: {block:?}"
        );
        assert!(
            block.contains(&expected_proxy_pass),
            "location = {path} block's own proxy_pass must match the documented \
             Forward Port ({port}); an exact-match location does not inherit \
             NPM's auto-generated proxy, so a stale port here silently breaks \
             the route: {block:?}"
        );
    }
}

#[test]
fn npm_advanced_tab_config_never_declares_log_format() {
    // A `log_format` directive is only legal at nginx's http context —
    // one level above what NPM's per-host Advanced tab can reach
    // (forkwright/epistole#104's structural blocker, see the comment
    // this test is paired with in DEPLOY.md). NPM rejects the whole
    // Advanced-tab config at save time if it's present, so catch a
    // future edit that pastes the Caddy-style redaction technique in
    // here instead of the access_log-off mitigation at build time
    // rather than at a live host's save click.
    assert!(
        !npm_advanced_tab_config().contains("log_format"),
        "log_format cannot be declared in NPM's per-host Advanced tab \
         (server-block scope); it belongs at http scope, out of this \
         repo's reach"
    );
}

#[test]
fn systemd_unit_tolerates_an_absent_env_file() {
    assert!(
        SERVICE_UNIT.contains("EnvironmentFile=-"),
        "a required EnvironmentFile makes the unit fail to start when a Phase 0 \
         deploy inlines its secrets instead"
    );
}

#[test]
fn runbook_does_not_promise_a_name_addressed_keyspace_directory() {
    assert!(
        !RUNBOOK.contains("data/subscribers"),
        "fjall stores keyspaces by numeric id, so no subscribers/ directory exists"
    );
}
