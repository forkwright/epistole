# AGENTS.md: epistole

Cross-tool agent guide for the epistole newsletter service. Complements
`CLAUDE.md` with the commands and boundaries every agent needs regardless
of model or client.

## Commands

Run these before every commit or push:

```bash
cargo test
cargo clippy --all-targets --all-features
cargo fmt --check
kanon lint . --summary
```

## Boundaries

- **No secrets in git.** Tokens, SMTP credentials, and HMAC keys live in
  `/etc/epistole.env` on the deploy host. Templated as `{{ ENV_VAR }}` in
  `epistole.toml`.
- **Push to the forkwright forge (`origin`), not GitHub directly.** The
  GitHub mirror is push-only via `kanon forge set-mirror`.
- **One `fjall::Keyspace` per process.** Out-of-process workers (imports,
  batch jobs, etc.) must talk to the running server over HTTP; opening the
  keyspace from another process violates the single-writer invariant.
- **No blocking code in async handlers.** The `await_holding_lock` lint is
  set to `deny`; keep CPU-bound work off the async runtime.

## Entry points

| File | Role |
|---|---|
| `src/lib.rs` | axum router builder (`pub fn router(...) -> Router`) |
| `src/store.rs` | fjall persistence layer (`Store`, `Subscriber`, `Send`, `Delivery`) |
| `src/token.rs` | HMAC-SHA256 confirm/unsubscribe token contract |
| `DEPLOY.md` | Operator runbook (systemd, reverse proxy, DNS, SMTP relay) |
| `CLAUDE.md` | Project conventions and fleet standards |
