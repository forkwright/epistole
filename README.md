# epistole

Self-hosted newsletter service for fleet web properties. ἐπιστολή - "letter sent."

Replaces Buttondown for any typikon-consuming site that wants a self-hosted newsletter. Configure the target domain in your instance config (see DEPLOY.md).

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

Subscriber flows, archive, operator send, and SMTP delivery are all functional.

- Full subscriber lifecycle: `/subscribe` -> confirm link -> `/confirm` -> `/unsubscribe`
- Archive: `/archive` lists past sends; `/archive/{id}` renders individual issues
- Operator: `POST /send` (bearer-gated) stores newsletter records and delivers them via `SmtpMailer` (`src/mailer.rs`), the production `Mailer` wired in `main.rs`
- Systemd unit and full reverse-proxy deploy runbook in [DEPLOY.md](DEPLOY.md)
- Subscriber-import tool and `<your-domain>` contact form cutover are follow-up items

See `CLAUDE.md` for boundaries and conventions.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
