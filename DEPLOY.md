# Epistole deploy runbook

End-to-end instructions for taking epistole from "Phase 0 substrate landed on main" to "letters.<your-domain> is live and the contact-page form posts to it."

Estimated operator time once secrets are in hand: **~30 minutes**, gated on DNS propagation and SMTP-relay account setup. None of these steps require Claude - they're operator-credential operations.

## What you'll need before you start

1. **SMTP relay account** - Postmark or Mailgun. Both have free tiers that cover hundreds of newsletter sends per month. Postmark's "Broadcasts" stream is the right product; Mailgun's "Sending" domain works too.
2. **DNS access** for `<your-domain>` (Cloudflare DNS dashboard).
3. **DKIM record** from the relay provider (their dashboard generates it).
4. **A box that runs systemd** and reaches the public internet on 443 - `<deploy-host>` (any box that runs systemd and reaches the internet on 443).

## Step 1 - Build + install the binary

```bash
cd ~/dev/epistole
env -u CARGO_TARGET_DIR cargo build --release
sudo install target/release/epistole /usr/local/bin/epistole
/usr/local/bin/epistole --version
```

The binary is ~25MB statically-linked. Build takes ~5 min on first compile, ~30s on rebuild.

## Step 2 - Set up the runtime user + data directory

```bash
sudo useradd --system --home /var/lib/epistole --shell /usr/sbin/nologin epistole
sudo install -d -o epistole -g epistole -m 0750 /var/lib/epistole
sudo install -d -o epistole -g epistole -m 0750 /var/lib/epistole/data
```

The fjall keyspace lives at `/var/lib/epistole/data/`. Restic backup target should include this path.

## Step 3 - Configure the SMTP relay

### Postmark path (recommended)

1. Sign up at postmarkapp.com → create a "Server" (e.g. "<Your Brand> Letters").
2. Add `<your-domain>` as a sender signature. Verify the SPF + DKIM records they hand you (drop into Cloudflare DNS).
3. Generate a server API token. `SMTP_USERNAME` AND `SMTP_PASSWORD` are both this same token.
4. Server → Webhooks → add a bounce webhook and a spam-complaint webhook, both pointed at your own translating endpoint (see "Bounce/complaint webhooks" below - Postmark's native payload shape is not what `/webhooks/delivery-events` accepts).

### Mailgun path

1. Add `mail.<your-domain>` (or `letters.<your-domain>`) as a sending domain.
2. Add Mailgun's MX, SPF, DKIM, DMARC records to Cloudflare DNS.
3. Generate an SMTP password from the domain's "Domain credentials" tab. `SMTP_USERNAME` is `postmaster@<sending-domain>` - a distinct value from `SMTP_PASSWORD`, unlike Postmark.
4. Sending → Webhooks → add `permanent_fail` and `complained` webhooks, same translating-endpoint caveat as above.

## Step 4 - Generate epistole secrets

```bash
TOKEN_SECRET=$(head -c 32 /dev/urandom | base64 -w 0)
SEND_AUTH_TOKEN=$(head -c 24 /dev/urandom | base64 -w 0)
WEBHOOK_AUTH_TOKEN=$(head -c 24 /dev/urandom | base64 -w 0)
echo "TOKEN_SECRET=$TOKEN_SECRET"
echo "SEND_AUTH_TOKEN=$SEND_AUTH_TOKEN"
echo "WEBHOOK_AUTH_TOKEN=$WEBHOOK_AUTH_TOKEN"
```

Save all three - you'll paste them into `/etc/epistole.env` in the next step. Losing them isn't catastrophic (regenerate, re-mint outstanding confirm links, accept the brief disruption) but you'll want them in your password manager.

`WEBHOOK_AUTH_TOKEN` is a bearer secret you invent - it authenticates whatever calls `POST /webhooks/delivery-events`, not a value the relay hands you. It's distinct from `SEND_AUTH_TOKEN` on purpose: a leaked webhook secret must not also authorize triggering a send.

## Step 5 - Install config + env file

```bash
sudo install -d -m 0755 /etc/epistole
sudo cp ~/dev/epistole/epistole.example.toml /etc/epistole/epistole.toml
sudoedit /etc/epistole/epistole.toml
```

Fill in:
- `bind = "127.0.0.1:9090"` (the reverse proxy fronts this; no public bind)
- `data_dir = "/var/lib/epistole/data"`
- `base_url = "https://letters.<your-domain>"`
- `send_cap_per_hour` / `send_cap_per_day` - not secrets, just numbers. Start conservative (the shipped example's 500/2000) and raise once you've confirmed the relay plan and list size support more.
- `[brand]` block with `name = "<Your Brand Name>"`, `from_address = "letters@<your-domain>"`, optional `reply_to = "contact@<your-domain>"`
- `[smtp]` block with the relay host, port, and username
- `trusted_proxies` — the address(es) NPM connects to epistole FROM, not
  the address it forwards TO. `["127.0.0.1"]` for the same-box topology
  this runbook uses; NPM's own LAN IP if it runs on a different host.
  Each entry accepts a CIDR range (`"10.0.0.0/24"`) instead of a single
  address if that host doesn't present one stable IP (a load-balanced
  NPM pool, for example) — this runbook's single-box topology doesn't
  need it, but the field isn't limited to single literals. Leaving this
  empty is safe but means `X-Forwarded-For` is never honored, so every
  visitor behind NPM collapses onto one rate-limit bucket — see
  `TrustedProxyExtractor` in `src/lib.rs`

Leave the four secrets — including both SMTP credentials — as environment
references — do not paste the values from step 4 into the toml:

```toml
token_secret = "${TOKEN_SECRET}"
send_auth_token = "${SEND_AUTH_TOKEN}"
webhook_auth_token = "${WEBHOOK_AUTH_TOKEN}"

[smtp]
username = "${SMTP_USERNAME}"
password = "${SMTP_PASSWORD}"
```

`Config::load` resolves each `${NAME}` from the process environment at
startup, then rejects placeholder or short values. Pasting a literal such as
`<TOKEN_SECRET from step 4>` is refused by the blocked-substring check in
`src/config.rs` and the service will not boot.

Keep the root keys (`bind`, `data_dir`, `base_url`, `token_secret`,
`send_auth_token`, `webhook_auth_token`, `send_cap_per_hour`,
`send_cap_per_day`, `trusted_proxies`) **above** the `[brand]` and `[smtp]`
headers. TOML assigns every key after a table header to that table, so a
root key placed below `[smtp]` loads as `smtp.<key>` and fails with `unknown
field`.

Now write the env file the unit reads, using the values from step 4:

```bash
sudo install -m 0600 -o root -g root /dev/null /etc/epistole.env
sudoedit /etc/epistole.env
```

```ini
TOKEN_SECRET=<TOKEN_SECRET from step 4>
SEND_AUTH_TOKEN=<SEND_AUTH_TOKEN from step 4>
WEBHOOK_AUTH_TOKEN=<WEBHOOK_AUTH_TOKEN from step 4>
SMTP_USERNAME=<relay username - Postmark: same token as SMTP_PASSWORD; Mailgun: postmaster@<sending-domain>>
SMTP_PASSWORD=<relay password / API token>
```

Lock the config down:

```bash
sudo chown root:epistole /etc/epistole/epistole.toml
sudo chmod 0640 /etc/epistole/epistole.toml
```

## Step 6 - Install the systemd unit

```bash
sudo install ~/dev/epistole/deploy/epistole.service /etc/systemd/system/epistole.service
sudo systemctl daemon-reload
sudo systemctl enable --now epistole.service
sudo systemctl status epistole.service
journalctl -u epistole.service -f -n 50
```

You should see:

```
INFO epistole listening addr=127.0.0.1:9090
```

Health probe locally:

```bash
curl -s http://127.0.0.1:9090/healthz
# -> ok
```

## Step 7 - DNS for letters.<your-domain>

Add to Cloudflare DNS (proxy ON - orange cloud):

```
letters.<your-domain>  CNAME  <deploy-host>.<domain>  proxied
```

If `<deploy-host>` is on Tailscale only, point to the operator's Cloudflare Tunnel CNAME. Verify:

```bash
dig +short letters.<your-domain>
```

## Step 8 - Reverse-proxy (NPM, on <reverse-proxy-host>)

> **Note**: the original runbook assumed Caddy. Your deploy topology may run **Nginx Proxy Manager (NPM)** (or another reverse proxy) at `<gateway-ip>:443`. NPM does TLS termination via a wildcard LAN cert + Cloudflare-fronted public certs. Adding a new public host is a UI-driven operation; the steps below are the click-path plus the fields that matter for security.

### 8a - Add the proxy host in NPM

Open NPM at `https://<reverse-proxy-host>` → **Hosts → Proxy Hosts → Add Proxy Host**.

**Details tab:**

| Field | Value |
|---|---|
| Domain Names | `letters.<your-domain>` |
| Scheme | `http` |
| Forward Hostname | `127.0.0.1` (epistole binds to loopback on the same box as NPM, OR the LAN IP of <deploy-host> if the reverse proxy is on a different host) |
| Forward Port | `9090` |
| Cache Assets | OFF |
| Block Common Exploits | **ON** |
| Websockets Support | OFF (epistole is plain HTTP) |
| Access List | (leave at default unless you want IP allowlisting) |

**SSL tab:**

| Field | Value |
|---|---|
| SSL Certificate | Request a new SSL certificate (Let's Encrypt) |
| Force SSL | **ON** |
| HTTP/2 Support | ON |
| HSTS Enabled | **ON** |
| HSTS Subdomains | OFF (only the `letters.` subdomain) |

**Advanced tab - paste the following Nginx custom config**:

```nginx
# === epistole hardening at the proxy edge ===

# === Cloudflare real-IP restoration ===
# CRITICAL: with Cloudflare orange-cloud in front of NPM, $remote_addr
# is the Cloudflare edge IP, NOT the visitor. Without these directives,
# X-Forwarded-For below would be set to the edge IP and one abusive
# client could exhaust the rate-limit bucket for unrelated visitors on
# the same edge. (Reaudit finding #32.)
#
# The CIDR list is Cloudflare's official trusted ranges; refresh from
# https://www.cloudflare.com/ips-v4 + /ips-v6 if Cloudflare expands.
set_real_ip_from 173.245.48.0/20;
set_real_ip_from 103.21.244.0/22;
set_real_ip_from 103.22.200.0/22;
set_real_ip_from 103.31.4.0/22;
set_real_ip_from 141.101.64.0/18;
set_real_ip_from 108.162.192.0/18;
set_real_ip_from 190.93.240.0/20;
set_real_ip_from 188.114.96.0/20;
set_real_ip_from 197.234.240.0/22;
set_real_ip_from 198.41.128.0/17;
set_real_ip_from 162.158.0.0/15;
set_real_ip_from 104.16.0.0/13;
set_real_ip_from 104.24.0.0/14;
set_real_ip_from 172.64.0.0/13;
set_real_ip_from 131.0.72.0/22;
set_real_ip_from 2400:cb00::/32;
set_real_ip_from 2606:4700::/32;
set_real_ip_from 2803:f800::/32;
set_real_ip_from 2405:b500::/32;
set_real_ip_from 2405:8100::/32;
set_real_ip_from 2a06:98c0::/29;
set_real_ip_from 2c0f:f248::/32;
real_ip_header CF-Connecting-IP;
real_ip_recursive on;

# After real-IP restoration, $remote_addr is the visitor IP (not the
# Cloudflare edge), so the X-Forwarded-For replacement below carries
# the real client identity to epistole.

# Defense-in-depth body cap. epistole's tower_http RequestBodyLimitLayer
# enforces per-route caps (4 KiB / 256 KiB), but rejecting at NPM saves
# the full TCP round-trip + tower middleware traversal.
client_max_body_size 256k;

# X-Forwarded-For trust: REPLACE the incoming chain rather than APPEND.
# Without this, a hostile client sets X-Forwarded-For: 1.2.3.4 in their
# request and NPM would forward it unchanged. proxy_set_header overrides
# any client-supplied value so the header NPM sends is always the real
# visitor IP.
#
# This is one half of the trust boundary, not the whole of it: epistole
# itself independently verifies that the peer connecting to it is listed
# in trusted_proxies (epistole.toml) before it honors this header at
# all - forkwright/epistole#67. A client that reaches epistole directly,
# bypassing NPM entirely, gets keyed on its own real socket peer no
# matter what X-Forwarded-For it sends; this NPM-side replace only
# matters for requests that DO come through NPM.
proxy_set_header X-Forwarded-For $remote_addr;
proxy_set_header X-Real-IP $remote_addr;
proxy_set_header X-Forwarded-Proto $scheme;
proxy_set_header X-Forwarded-Host $host;

# Conservative timeouts. epistole responds to subscribe/confirm in ms;
# anything slower is either a slow client (drop) or a runaway handler
# (which tower_http TimeoutLayer would have already aborted at 10s).
proxy_connect_timeout 5s;
proxy_send_timeout 15s;
proxy_read_timeout 15s;

# Disable buffering for the streaming response on /healthz; harmless
# elsewhere (handler responses are tiny).
proxy_buffering off;

# Strip headers that shouldn't be coming through.
proxy_set_header Host $host;
proxy_hide_header X-Powered-By;

# Forensic correlation: forward the request id into epistole logs.
proxy_set_header X-Request-Id $request_id;

# Hardening response headers (NPM sets HSTS via the SSL tab; the rest
# duplicate what epistole could set itself but adding here makes the
# proxy the single source of truth):
add_header X-Content-Type-Options "nosniff" always;
add_header X-Frame-Options "DENY" always;
add_header Referrer-Policy "strict-origin-when-cross-origin" always;
add_header Permissions-Policy "geolocation=(), microphone=(), camera=()" always;

# === Token-query-parameter redaction (forkwright/epistole#104) ===
#
# GET /confirm, GET /unsubscribe, and POST /unsubscribe/one-click all
# carry the signed capability token — and, base64-nested inside it, the
# subscriber's email — as a `token` query parameter. NPM's default
# access log format records the complete request line verbatim, so
# every hit on these three routes would otherwise persist the token to
# <reverse-proxy-data>/logs/proxy-host-*_access.log, the same file
# section 8b's CrowdSec scenario reads. deploy/Caddyfile.snippet closes
# the identical exposure for Caddy (forkwright/epistole#66) with a
# redacting log format, but that mechanism can only be declared one
# nginx context above this Advanced tab — it is not reachable from a
# per-host field. The mitigation reachable from here is turning logging
# off for just these three exact routes:
location = /confirm {
    access_log off;
    proxy_pass http://127.0.0.1:9090;
}
location = /unsubscribe {
    access_log off;
    proxy_pass http://127.0.0.1:9090;
}
location = /unsubscribe/one-click {
    access_log off;
    proxy_pass http://127.0.0.1:9090;
}
# Each `location =` above is an EXACT match, so — unlike a `location /`
# prefix block — it does NOT fall back to NPM's auto-generated proxy
# and must therefore name its own proxy_pass, duplicating the Forward
# Port from the Details tab above (currently 9090). That duplication is
# a drift risk on its own: tests/deploy_contract.rs's
# `npm_token_routes_redact_access_log_and_agree_on_the_forward_port`
# fails the moment this stops matching. proxy_set_header directives are
# deliberately NOT repeated inside these blocks: nginx inherits the
# full server-level set defined above (Cloudflare real-IP restoration
# included) into a location UNLESS that location defines its own — so
# redeclaring even one of them here would silently drop the rest.
```

Save → enable → wait for the cert challenge.

**Tradeoff, stated explicitly:** these three routes stop appearing in
`<reverse-proxy-data>/logs/proxy-host-*_access.log` at all, so section
8b's `forkwright/epistole-abuse` CrowdSec scenario cannot see abuse
traffic against them specifically — epistole's own `GovernorLayer`
(6 req/min per key, `src/lib.rs`) still rate-limits every one of these
routes regardless of what the proxy log sees. The alternative that
keeps full visibility is administrator-side and out of this repo's
reach: adding a redacting format at NPM's global nginx scope (one
context above what any per-host Advanced tab can declare) and pointing
this host's access log at it by name. That path is not shipped here
because it cannot be — there is no per-host field it can be pasted
into, and no config this repo controls that reaches NPM's global scope.

End-to-end probes:

```bash
# 1. Liveness - should return "ok"
curl -sf https://letters.<your-domain>/healthz

# 2. Real-IP-restoration probe - must surface the visitor IP, not a
#    Cloudflare edge. Trigger from a known IP, then check both layers.
curl -sf https://letters.<your-domain>/subscribe \
  -d email=runbook-probe@example.com -X POST > /dev/null

# 2a. NPM access log shows the visitor IP (not 162.158.x.x / 172.64.x.x)
sudo tail -1 <reverse-proxy-data>/logs/proxy-host-*_access.log

# 2b. epistole journal shows the visitor IP in the rate-limit key
#     (look for the email_hmac_short field - the request itself
#      doesn't echo IP, but the corresponding NPM line should)
sudo journalctl --user -u <deploy-host>-daimon-* 2>/dev/null || \
  sudo journalctl -u epistole.service --since "1 min ago" | grep "confirm link minted"
```

If NPM logs show a Cloudflare edge IP (162.158.x.x or 172.64.x.x range), the `set_real_ip_from` step didn't take - re-check the CIDR list against current Cloudflare advertised ranges.

### 8b - Verify CrowdSec is parsing the new proxy host

CrowdSec already reads NPM's per-proxy-host log files (`<reverse-proxy-data>/logs/proxy-host-*_access.log`) under acquis.yaml's nginx source. The custom `forkwright/epistole-abuse` scenario triggers when one IP gets 5+ 4xx responses (401/413/429/400) from `letters.<your-domain>` inside 60s, banning at the firewall for 4 hours.

Confirm the scenario is loaded:

```bash
sudo podman exec crowdsec cscli scenarios list | grep epistole
# forkwright/epistole-abuse  enabled,local
```

Trigger from a test IP (do NOT do this from your real IP - you'll ban yourself for 4h):

```bash
for i in {1..10}; do curl -s -X POST https://letters.<your-domain>/subscribe \
  -H "X-Forwarded-For: 198.51.100.99" -d email=test@example.com -o /dev/null -w "%{http_code}\n"; done
sudo podman exec crowdsec cscli decisions list | grep 198.51.100
```

(NPM strips X-Forwarded-For per the snippet above, so this test does not fire - to test, hit the endpoint directly from a test source IP without spoofing the header.)

## Step 8c - Cloudflare edge protection (recommended)

epistole sits behind Cloudflare proxying for `letters.<your-domain>` (orange cloud). Cloudflare provides DDoS scrubbing and bot-fight by default; two Page Rules / WAF rules tighten the public-internet attack surface before requests even reach NPM.

### Rate-limit rule

Cloudflare Dashboard → **Security → WAF → Rate limiting rules → Create rule**.

| Field | Value |
|---|---|
| Rule name | `epistole-subscribe` |
| Field | `URI Path` equals `/subscribe` AND `hostname` equals `letters.<your-domain>` |
| Characteristics | `IP` |
| Requests per period | `5` |
| Period | `1 minute` |
| Action | `Block` |
| Duration | `5 minutes` |

This is the **first** line of defense; epistole's `tower_governor` (6/min) is the second; CrowdSec's `forkwright/epistole-abuse` (5 4xx responses → 4h ban) is the third.

### Bot fight mode

Cloudflare Dashboard → **Security → Bots → Bot Fight Mode** → ON.

Catches automated scrapers + low-effort form-fillers; legitimate visitors are unaffected.

### Optional: Cloudflare Access for /send

If you want zero-trust auth on the operator endpoint (defense beyond the bearer token):

Cloudflare Dashboard → **Zero Trust → Access → Applications → Add an application → Self-hosted**.

| Field | Value |
|---|---|
| Application Name | `epistole-send` |
| Subdomain | `letters` |
| Domain | `<your-domain>` |
| Path | `/send` |
| Identity providers | One-time PIN to your email |

After enabling, `POST /send` requires a CF-Access JWT in addition to the bearer token. Even a leaked `send_auth_token` is unusable without your CF identity.

## Step 9 - DMARC record (recommended)

```
_dmarc.<your-domain>  TXT  "v=DMARC1; p=quarantine; rua=mailto:dmarc@<your-domain>; aspf=r; adkim=r"
```

This complements the SPF + DKIM records the relay provider gave you. `p=quarantine` is the safe middle ground; revisit `p=reject` after a week of clean DKIM.

## Step 10 - Smoke test the full flow

```bash
# Subscribe (substitute your real email — you need to receive this one)
curl -sf -X POST https://letters.<your-domain>/subscribe \
  -d email=test@example.com

# Confirm the mail went out. The line carries an HMAC of the address, not
# the address itself or the token - the confirm URL is not recoverable
# from the journal by design.
journalctl -u epistole.service | grep "confirm link mailed"
```

Check the test inbox and click the confirm link. `GET /confirm?token=...`
previews (no write); the interstitial's own form does the `POST /confirm`
that actually activates the subscriber (forkwright/epistole#68).

Then smoke-test a send. `send_id` must be a valid ULID (Crockford Base32,
26 chars) — any fixed one works for a one-off smoke test, since `/send`'s
idempotency keys on this value:

```bash
curl -sf -X POST https://letters.<your-domain>/send \
  -H "Authorization: Bearer $SEND_AUTH_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"send_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","subject":"Smoke test","markdown":"# It works"}'
```

The reply's `sent` count should be 1 (the test address you just confirmed).
Check the same inbox for the newsletter, including a working `Unsubscribe`
link in the footer — that link mints an unsubscribe token good for years
(`UNSUBSCRIBE_LINK_TTL` in `src/handlers/send.rs`), not the confirm link's
24h window. Re-running the exact same `curl` command is safe to try: the
reply's `sent` becomes 0 and `already_delivered` becomes 1 — the second
call recorded no new delivery.

Subscriber state is not inspectable from the filesystem either. fjall stores
keyspaces by numeric id under `<data_dir>/keyspaces/<id>/`, not by name, so
no `subscribers/` directory exists to dump. Read subscriber state through
the service, not the data directory.

## Step 11 - Cut over <your-domain>'s contact form

```bash
cd ~/dev/<your-site>
git checkout -b cutover/buttondown-to-epistole
```

Edit `content/contact.md`:

```diff
-<form
-  action="https://buttondown.com/api/emails/embed-subscribe/Ardent_Leatherworks"
-  method="post"
-  class="buttondown-form"
->
+<form
+  action="https://letters.<your-domain>/subscribe"
+  method="post"
+  class="newsletter-form"
+>
```

Edit `_headers` (under `Content-Security-Policy: form-action`) to swap `https://buttondown.com` → `https://letters.<your-domain>`.

Edit `content/privacy.md` - replace any "Buttondown" reference with epistole's posture (zero third-party newsletter providers; subscriber list lives on operator's own server).

Open PR + bypass-merge per the typikon flow. Cloudflare auto-deploys; verify the form submission flows end-to-end via a test subscribe.

## Step 12 - Subscriber migration

If there are existing Buttondown subscribers:

```bash
# Export from Buttondown UI: Settings → Export → CSV (email column required)

# Import via the helper that lands in Phase 3:
~/dev/epistole/bin/epistole-import buttondown.csv
```

Phase 3 ships `bin/epistole-import` (not in scope for the Phase 0 substrate). Until it lands, manual migration via direct fjall writes is possible but discouraged.

## Backups

Add to your backup script (e.g. `~/.local/bin/<deploy-host>-backup`):

```bash
restic backup /var/lib/epistole --tag epistole
```

The fjall keyspace is small (~10 MB at any reasonable subscriber count); daily backups are fine.

## Rollback

If anything breaks during cutover:

1. Revert the contact.md form change → `git revert <cutover-sha>`.
2. Buttondown stays the active newsletter provider (don't disable the Buttondown account for ~30 days post-cutover).
3. epistole continues collecting any subscribers that hit it; merge their list later.

## token_secret rotation

`token_secret` signs the confirm + unsubscribe links emailed to subscribers. Rotate as a periodic hygiene measure (annually) or immediately on suspected leak. Rotation invalidates all outstanding confirm + active-unsubscribe tokens; **active subscribers stay subscribed** (Active state is in the fjall keyspace, not in tokens), but visitors holding unclicked confirm links must re-subscribe and active subscribers wanting to unsubscribe must request a fresh link.

```bash
# 1. Generate a new 32-byte secret (matches Step 4's generation):
NEW_TOKEN_SECRET=$(head -c 32 /dev/urandom | base64 -w 0)

# 2. Update /etc/epistole/epistole.toml (file is 0640 root:epistole):
sudoedit /etc/epistole/epistole.toml
# Replace the token_secret = "..." line.

# 3. Restart the service to pick up the new secret:
sudo systemctl restart epistole.service

# 4. Verify the unit is healthy:
systemctl status epistole.service
journalctl -u epistole.service -n 50 --no-pager
```

Operational notes:

- **Confirm-link impact**: any visitor who clicked Subscribe before the rotation but hasn't clicked their confirm link yet will get a "token invalid" error on their stale link. The fix on their side is identical to a normal sign-up: re-subscribe to receive a fresh confirm link.
- **Active-unsubscribe impact**: pre-rotation unsubscribe links emailed in past Sends become invalid. Subscribers needing to unsubscribe land on a "token invalid" page and must request a fresh unsubscribe link (Phase 2 wiring will surface this via a "request new link" form; until then, they'd have to email the operator).
- **Active subscribers are unaffected**: their Active state is keyed by normalized email in the fjall `subscribers` partition; no token re-issue needed.

The rotate-and-restart approach assumes outstanding unclicked confirm links are acceptable to invalidate. If that becomes operationally painful, a future change can add a `token_secret_previous` config field that `verify()` falls back to during a rotation window. The original analysis in forkwright/epistole#3 concluded the maintenance cost of a two-secret window outweighed its benefit at Phase 0 scale; revisit that conclusion when confirm-link loss surfaces as real friction.

### Upgrading past the consent-generation token change (#65/#68)

The confirm/unsubscribe token wire format gained a fourth field (consent
generation) and the mutation moved from `GET` to `POST` behind a
same-origin interstitial. Any token signed under the old three-field
format fails to parse under the new verifier and lands on the
invalid-link page — the same visible effect as a `token_secret` rotation
above, but it happens once, on the deploy that carries this change, not
on an operator-triggered schedule. No `data_dir` migration is needed:
existing `Subscriber` rows deserialize with `generation = 0`, which is
the same baseline a brand-new address starts at, so restarting onto the
new binary is the entire upgrade step.

## Bounce/complaint webhooks

`POST /webhooks/delivery-events` records a bounce or complaint outcome
against an existing delivery row and, for a complaint or a hard bounce,
suppresses the subscriber (Active → Unsubscribed) the same way a visitor's
own unsubscribe click does. It is bearer-authenticated by `webhook_auth_token`
and takes a small provider-agnostic body:

```json
{"send_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "email": "reader@example.com", "kind": "bounce", "hard": true}
```

Neither Postmark's nor Mailgun's native webhook payload matches this shape —
picking a relay account and wiring its real webhook is a Step 3 operator
decision, this runbook doesn't make it for you. Point the relay's webhook at
a small translating endpoint (a Cloudflare Worker, an NPM location block
that reshapes the body, or anything that speaks the relay's format on one
side and this JSON shape on the other) rather than the relay's raw URL.

## What's NOT in this runbook

- **A bounce/complaint webhook adapter for a specific relay** - `POST /webhooks/delivery-events`'s ledger-update and suppression logic is live; translating Postmark's or Mailgun's native webhook JSON into its provider-agnostic shape is the operator-credential step described above.
- **Archive feed/import polish** - the `/archive` index/detail pages walk the sends ledger; `/archive.xml` and import tooling remain follow-up work.
- **bin/epistole-import** - tracked separately; Phase 3.

These are all additive; the substrate landed in Phase 0 plus the SMTP
wire-up above is sufficient to run a real newsletter.
