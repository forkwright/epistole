# epistole

Sovereign newsletter service for fleet web properties. ἐπιστολή - "letter sent."

Replaces Buttondown for [ardentleatherworks.com](https://ardentleatherworks.com) and any future typikon-consuming site that wants a self-hosted newsletter.

## What it is

A standalone Rust service:

- **public**: `/subscribe`, `/confirm`, `/unsubscribe`, `/archive`
- **operator**: `/send` (auth-gated; renders Markdown to HTML to SMTP relay to ledger)
- **storage**: fjall LSM-tree (subscriber list, send history, delivery ledger)
- **deliverability**: SMTP relay through Postmark or Mailgun - DKIM/SPF/DMARC live on the relay outbound IP, not on the host

## Stack

| Concern | Crate |
|---|---|
| HTTP framework | `axum` |
| Persistence | `fjall` |
| HTML templates | `maud` |
| SMTP | `lettre` |
| TLS | `rustls` |
| Errors | `snafu` |
| Tracing | `tracing` |
| Secret handling | `secrecy` |

## Status

Phase 0 - substrate scaffold. Code does not yet build a working server. Subsequent commits land:

1. Storage layer (subscribers, tokens, sends, delivery ledger)
2. Public endpoints + maud templates for confirm/unsubscribe/archive pages
3. Operator endpoint (`/send`) + markdown rendering
4. SMTP relay integration (Postmark/Mailgun adapter) - tracked at issue #1
5. Subscriber-import tool (`bin/epistole-import` for migrating from Buttondown)
6. Systemd unit + reverse-proxy snippet for aletheia deploy (see DEPLOY.md for NPM topology)
7. Cutover: replace Buttondown form on ardent's contact page

See `CLAUDE.md` for boundaries and conventions.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
