# Epistole deploy runbook

End-to-end instructions for taking epistole from "Phase 0 substrate landed on main" to "letters.ardentleatherworks.com is live and the contact-page form posts to it."

Estimated operator time once secrets are in hand: **~30 minutes**, gated mostly on DNS propagation and SMTP-relay account setup. None of these steps require Claude — they're operator-credential operations.

## What you'll need before you start

1. **SMTP relay account** — Postmark or Mailgun. Both have free tiers that cover hundreds of newsletter sends per month. Postmark's "Broadcasts" stream is the right product; Mailgun's "Sending" domain works too.
2. **DNS access** for `ardentleatherworks.com` (Cloudflare DNS dashboard).
3. **DKIM record** from the relay provider (their dashboard generates it).
4. **A box that runs systemd** and reaches the public internet on 443 — `aletheia` or `menos` both qualify. The runbook below assumes aletheia.

## Step 1 — Build + install the binary

```bash
cd ~/dev/epistole
env -u CARGO_TARGET_DIR cargo build --release
sudo install target/release/epistole /usr/local/bin/epistole
/usr/local/bin/epistole --version
```

The binary is ~25MB statically-linked. Build takes ~5 min on first compile, ~30s on rebuild.

## Step 2 — Set up the runtime user + data directory

```bash
sudo useradd --system --home /var/lib/epistole --shell /usr/sbin/nologin epistole
sudo install -d -o epistole -g epistole -m 0750 /var/lib/epistole
sudo install -d -o epistole -g epistole -m 0750 /var/lib/epistole/data
```

The fjall keyspace lives at `/var/lib/epistole/data/`. Restic backup target should include this path going forward.

## Step 3 — Configure the SMTP relay

### Postmark path (recommended)

1. Sign up at postmarkapp.com → create a "Server" (e.g. "Ardent Letters").
2. Add `ardentleatherworks.com` as a sender signature. Verify the SPF + DKIM records they hand you (drop into Cloudflare DNS).
3. Generate a server API token. Username AND password for SMTP both = the API token.

### Mailgun path

1. Add `mail.ardentleatherworks.com` (or `letters.ardentleatherworks.com`) as a sending domain.
2. Add Mailgun's MX, SPF, DKIM, DMARC records to Cloudflare DNS.
3. Generate an SMTP password from the domain's "Domain credentials" tab.

## Step 4 — Generate epistole secrets

```bash
TOKEN_SECRET=$(head -c 32 /dev/urandom | base64 -w 0)
SEND_AUTH_TOKEN=$(head -c 24 /dev/urandom | base64 -w 0)
echo "TOKEN_SECRET=$TOKEN_SECRET"
echo "SEND_AUTH_TOKEN=$SEND_AUTH_TOKEN"
```

Save both — you'll paste them into `/etc/epistole.env` in the next step. Losing them isn't catastrophic (regenerate, re-mint pending tokens, accept the brief disruption) but you'll want them in your password manager.

## Step 5 — Install config + env file

```bash
sudo install -d -m 0755 /etc/epistole
sudo cp ~/dev/epistole/epistole.example.toml /etc/epistole/epistole.toml
sudoedit /etc/epistole/epistole.toml
```

Fill in:
- `bind = "127.0.0.1:9090"` (Caddy reverse-proxies; no public bind)
- `data_dir = "/var/lib/epistole/data"`
- `base_url = "https://letters.ardentleatherworks.com"`
- `[brand]` block with `name = "Ardent Leatherworks"`, `from_address = "letters@ardentleatherworks.com"`, optional `reply_to = "contact@ardentleatherworks.com"`
- `[smtp]` block with the Postmark/Mailgun credentials
- `token_secret = "<TOKEN_SECRET from step 4>"`
- `send_auth_token = "<SEND_AUTH_TOKEN from step 4>"`

Lock it down:

```bash
sudo chown root:epistole /etc/epistole/epistole.toml
sudo chmod 0640 /etc/epistole/epistole.toml
```

## Step 6 — Install the systemd unit

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

## Step 7 — DNS for letters.ardentleatherworks.com

Add to Cloudflare DNS (proxy ON — orange cloud):

```
letters.ardentleatherworks.com  CNAME  <aletheia-host>.<domain>  proxied
```

If aletheia is on Tailscale only, point to the operator's Cloudflare Tunnel CNAME. Verify:

```bash
dig +short letters.ardentleatherworks.com
```

## Step 8 — Reverse-proxy (NPM, on menos)

> **Note**: the original runbook assumed Caddy. The actual menos topology runs **Nginx Proxy Manager (NPM)** at the gateway pod (`100.74.109.2:443`). NPM does TLS termination via a wildcard `*.lan` (LAN) cert + Cloudflare-fronted public certs. Adding a new public host is a UI-driven operation; the steps below are the click-path plus the fields that matter for security.

### 8a — Add the proxy host in NPM

Open NPM at `https://npm.lan` → **Hosts → Proxy Hosts → Add Proxy Host**.

**Details tab:**

| Field | Value |
|---|---|
| Domain Names | `letters.ardentleatherworks.com` |
| Scheme | `http` |
| Forward Hostname | `127.0.0.1` (epistole binds to loopback on the same box as NPM, OR the LAN IP of menos if NPM is on a different host) |
| Forward Port | `9091` |
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

**Advanced tab — paste the following Nginx custom config**:

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
# request and tower_governor's TrustedProxyExtractor would use 1.2.3.4
# as the rate-limit key — bypassing per-IP enforcement. proxy_set_header
# overrides any client-supplied value.
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
```

Save → enable → wait for the cert challenge.

End-to-end probes:

```bash
# 1. Liveness — should return "ok"
curl -sf https://letters.ardentleatherworks.com/healthz

# 2. Real-IP-restoration probe — must surface the visitor IP, not a
#    Cloudflare edge. Trigger from a known IP, then check both layers.
curl -sf https://letters.ardentleatherworks.com/subscribe \
  -d email=runbook-probe@example.com -X POST > /dev/null

# 2a. NPM access log shows the visitor IP (not 162.158.x.x / 172.64.x.x)
sudo tail -1 /storage/npm/data/logs/proxy-host-*_access.log

# 2b. epistole journal shows the visitor IP in the rate-limit key
#     (look for the email_hmac_short field — the request itself
#      doesn't echo IP, but the corresponding NPM line should)
sudo journalctl --user -u menos-daimon-* 2>/dev/null || \
  sudo journalctl -u epistole.service --since "1 min ago" | grep "confirm link minted"
```

If NPM logs show a Cloudflare edge IP (162.158.x.x or 172.64.x.x range), the `set_real_ip_from` step didn't take — re-check the CIDR list against current Cloudflare advertised ranges.

### 8b — Verify CrowdSec is parsing the new proxy host

CrowdSec already reads NPM's per-proxy-host log files (`/data/npm/data/logs/proxy-host-*_access.log`) under acquis.yaml's nginx source. The custom `forkwright/epistole-abuse` scenario triggers when one IP gets 5+ 4xx responses (401/413/429/400) from `letters.ardentleatherworks.com` inside 60s, banning at the firewall for 4 hours.

Confirm the scenario is loaded:

```bash
sudo podman exec crowdsec cscli scenarios list | grep epistole
# forkwright/epistole-abuse  enabled,local
```

Trigger from a test IP (do NOT do this from your real IP — you'll ban yourself for 4h):

```bash
for i in {1..10}; do curl -s -X POST https://letters.ardentleatherworks.com/subscribe \
  -H "X-Forwarded-For: 198.51.100.99" -d email=test@example.com -o /dev/null -w "%{http_code}\n"; done
sudo podman exec crowdsec cscli decisions list | grep 198.51.100
```

(NPM strips X-Forwarded-For per the snippet above, so this test won't actually fire — to test, hit the endpoint directly from a test source IP rather than spoofing the header.)

## Step 8c — Cloudflare edge protection (recommended)

epistole sits behind Cloudflare proxying for `letters.ardentleatherworks.com` (orange cloud). Cloudflare provides DDoS scrubbing and bot-fight by default; two Page Rules / WAF rules tighten the public-internet attack surface before requests even reach NPM.

### Rate-limit rule

Cloudflare Dashboard → **Security → WAF → Rate limiting rules → Create rule**.

| Field | Value |
|---|---|
| Rule name | `epistole-subscribe` |
| Field | `URI Path` equals `/subscribe` AND `hostname` equals `letters.ardentleatherworks.com` |
| Characteristics | `IP` |
| Requests per period | `5` |
| Period | `1 minute` |
| Action | `Block` |
| Duration | `5 minutes` |

This is the **first** line of defense; epistole's `tower_governor` (6/min) is the second; CrowdSec's `forkwright/epistole-abuse` (5 4xx responses → 4h ban) is the third.

### Bot Fight Mode

Cloudflare Dashboard → **Security → Bots → Bot Fight Mode** → ON.

Catches automated scrapers + low-effort form-fillers; legitimate visitors are unaffected.

### Optional: Cloudflare Access for /send

If you want zero-trust auth on the operator endpoint (defense beyond the bearer token):

Cloudflare Dashboard → **Zero Trust → Access → Applications → Add an application → Self-hosted**.

| Field | Value |
|---|---|
| Application Name | `epistole-send` |
| Subdomain | `letters` |
| Domain | `ardentleatherworks.com` |
| Path | `/send` |
| Identity providers | One-time PIN to your email |

After enabling, `POST /send` requires a CF-Access JWT in addition to the bearer token. Even a leaked `send_auth_token` is unusable without your CF identity.

## Step 9 — DMARC record (recommended)

```
_dmarc.ardentleatherworks.com  TXT  "v=DMARC1; p=quarantine; rua=mailto:dmarc@ardentleatherworks.com; aspf=r; adkim=r"
```

This complements the SPF + DKIM records the relay provider gave you. `p=quarantine` is the safe middle ground; revisit `p=reject` after a week of clean DKIM.

## Step 10 — Smoke test the full flow

```bash
# Subscribe (substitute your real email)
curl -sf -X POST https://letters.ardentleatherworks.com/subscribe \
  -d email=test@example.com

# Look for the confirm URL in the journal — Phase 0 logs it instead of mailing
journalctl -u epistole.service | grep "confirm link minted"

# Open the confirm URL in a browser. Should see the "Subscribed." page.

# Verify subscriber state
fjall-tools dump /var/lib/epistole/data/subscribers/ | jq
```

If Phase 2 (forkwright/epistole#1) has landed, the confirm email will arrive in the test inbox via the SMTP relay.

## Step 11 — Cut over ardent's contact form

```bash
cd ~/dev/ardent-site
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
+  action="https://letters.ardentleatherworks.com/subscribe"
+  method="post"
+  class="newsletter-form"
+>
```

Edit `_headers` (under `Content-Security-Policy: form-action`) to swap `https://buttondown.com` → `https://letters.ardentleatherworks.com`.

Edit `content/privacy.md` — replace any "Buttondown" reference with epistole's posture (zero third-party newsletter providers; subscriber list lives on operator's own server).

Open PR + bypass-merge per the typikon flow. Cloudflare auto-deploys; verify the form submission flows end-to-end via a test subscribe.

## Step 12 — Subscriber migration

If there are existing Buttondown subscribers:

```bash
# Export from Buttondown UI: Settings → Export → CSV (email column required)

# Import via the helper that lands in Phase 3:
~/dev/epistole/bin/epistole-import buttondown.csv
```

Phase 3 ships `bin/epistole-import` (not in scope for the Phase 0 substrate). Until it lands, manual migration via direct fjall writes is possible but discouraged.

## Backups

Add to `~/.local/bin/menos-backup`:

```bash
restic backup /var/lib/epistole --tag epistole
```

The fjall keyspace is small (~10 MB at any reasonable subscriber count); daily backups are fine.

## Rollback

If anything breaks during cutover:

1. Revert the contact.md form change → `git revert <cutover-sha>`.
2. Buttondown stays the active newsletter provider (don't disable the Buttondown account for ~30 days post-cutover).
3. epistole continues collecting any subscribers that hit it; merge their list later.

## What's NOT in this runbook

- **Phase 2 wiring** (lettre SMTP relay) — tracked at forkwright/epistole#1. The Phase 0 build logs the confirm URL instead of mailing. Subscribe + confirm flows work; the operator just has to copy-paste the link in early days.
- **Phase 2 archive page walking** — tracked at forkwright/epistole#2. `/archive` returns a stub today.
- **bin/epistole-import** — tracked separately; Phase 3.

These are all additive; the substrate landed in Phase 0 is sufficient to start collecting subscribers + composing Sends, even before the SMTP wire-up.
