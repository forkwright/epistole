# ARCHITECTURE.md: epistole

## Overview

epistole is a sovereign newsletter service. Visitors submit an email via
`POST /subscribe`, receive a time-limited confirm link, and become `Active`
subscribers after clicking it. Active subscribers can later unsubscribe via
`GET /unsubscribe`. The operator creates newsletter issues via `POST /send`,
which stores a rendered HTML record; SMTP delivery to recipients is not yet
wired.

## HTTP routes

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/healthz` | public | Liveness probe |
| POST | `/subscribe` | public | Accept email, mint confirm token |
| GET | `/confirm` | public | Validate token, activate subscriber |
| GET | `/unsubscribe` | public | Validate token, deactivate subscriber |
| GET | `/archive` | public | List past sends |
| GET | `/archive/:id` | public | Single send detail page |
| POST | `/send` | bearer | Create a new send (markdown → HTML) |

Public routes share a per-IP rate limit (6 requests per 60 seconds). `/send`
is un-rate-limited but bearer-gated.

## Storage model

One `fjall::Keyspace` with three partitions:

- **`subscribers`** - keyed by lowercased email. Value is a JSON-encoded
  `Subscriber` record (`state`, `created_at`, `confirmed_at`,
  `unsubscribed_at`).
- **`sends`** - keyed by ULID. Value is a JSON-encoded `Send` record
  (`subject`, `body_html`, `sent_at`).
- **`deliveries`** - keyed by `<send_id>/<email>`. Value is a JSON-encoded
  `Delivery` record (`status`, `at`, `error`).

**Single-writer invariant:** only the running server process opens the
keyspace. Out-of-process tools must use HTTP.

## Security boundaries

- **Bearer auth on `/send`.** The presented `Authorization: Bearer <token>`
  is SHA256-hashed and compared against the expected hash in constant time
  via `subtle::ConstantTimeEq`. Authentication happens before any body
  collection or JSON parsing.
- **HMAC-SHA256 tokens.** Confirm and unsubscribe links carry signed,
  time-limited tokens (`base64url(payload).base64url(signature)`). Payload
  contains `kind|email_b64|exp_unix`. Verification rejects any signature
  mismatch, parse failure, or expiry with a single coarse error type to
  avoid timing oracles.
- **Timing equalization in `/subscribe`.** Every valid submission performs
  the same token-minting work regardless of whether the email is new,
  already active, or already unsubscribed, so an attacker cannot distinguish
  subscriber membership by response time.
- **Email validation.** The `email_address` crate parses with strict options:
  no display text, required TLD, no domain literals. Maximum accepted length
  is 254 bytes.
- **Body size caps.** `SUBSCRIBE_BODY_LIMIT` is 4 KiB; `SEND_BODY_LIMIT` is
  256 KiB. Enforced by `RequestBodyLimitLayer` and re-checked inside
  handlers.
- **HTML sanitization.** Markdown bodies are rendered to HTML with
  `pulldown-cmark`, then passed through `ammonia::clean` to strip
  non-allowlisted URL schemes (e.g. `javascript:`, `data:`).

## Deployment

- **Host:** instance-configured host (`letters.<your-domain>`).
- **TLS:** reverse proxy (Caddy, NPM, etc.) terminates TLS.
- **Process:** systemd service (`epistole.service`) binding to loopback.
- **Relay:** SMTP outbound via Postmark or Mailgun.
