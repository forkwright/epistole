<!--
scope: epistole project conventions
defers_to: none
tightens: Rust service development, deployment, and CI expectations for this repo
-->

# CLAUDE.md: epistole

Sovereign newsletter service for fleet web properties. Replaces Buttondown for any typikon-consuming site that wants a self-hosted newsletter. The target domain is instance-config.

The name is Greek: ἐπιστολή - "letter, epistle, dispatch sent." Newsletters are letters; the verb form means "send to."

## What it is

A self-contained Rust service that owns:

- subscriber list (email + opt-in state + token + audit timestamps)
- delivery ledger (per-send-per-recipient outcome history)
- public endpoints: `/subscribe`, `/confirm`, `/unsubscribe`, `/archive`
- operator endpoint: `/send` (auth-gated; markdown body -> rendered HTML -> SMTP relay -> ledger)

## Stack

- **axum** - HTTP framework
- **fjall** - embedded LSM-tree store (single-writer per keyspace; matches fleet pattern)
- **maud** - HTML templates (compile-time, type-safe)
- **lettre** - SMTP outbound; relays through Postmark or Mailgun (deliverability is non-negotiable)
- **rustls** - TLS

Deliverability boundary: epistole does not directly deliver to recipient inboxes. It relays via Postmark/Mailgun (low-volume free tier; both rate-limited but plenty for the kind of newsletters this fleet sends). DKIM/SPF/DMARC live on the relay's outbound IP, not on the deploy host.

## Where things live

- Local dev: `~/dev/epistole`
- GitHub: `github.com/forkwright/epistole` (canonical hosting — `origin`; not a forge mirror)
- Production: `letters.<your-domain>` (behind a reverse proxy; see DEPLOY.md; runs as a systemd service)

## Boundaries

- Push directly to `origin` (`github.com/forkwright/epistole`) — GitHub is canonical hosting for this repo, not a forge mirror.
- Never commit secrets - Postmark API tokens, SMTP creds, HMAC keys, etc. live in `/etc/epistole.env` (0600 root:root) on the deploy host; templated through `{{ ENV_VAR }}` in `epistole.toml`.
- Never write blocking code in async contexts (the `await_holding_lock` lint catches the obvious cases).
- All persisted writes go through one `fjall::Keyspace` - out-of-process workers (when added) must talk to epistole via HTTP, not by opening the keyspace directly.

## Standards

Same as the rest of the Rust fleet - see `kanon/crates/basanos/standards/STANDARDS.md`. Run `kanon lint . --summary` before push; `kanon gate --stamp` before opening a PR.
