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

## Step 8 — Caddy reverse-proxy

```bash
sudo cp ~/dev/epistole/deploy/Caddyfile.snippet /etc/caddy/sites-enabled/letters-ardentleatherworks.caddy
sudo sed -i 's|<consumer-domain>|ardentleatherworks.com|g' /etc/caddy/sites-enabled/letters-ardentleatherworks.caddy
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

End-to-end probe:

```bash
curl -sf https://letters.ardentleatherworks.com/healthz
# -> ok
```

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
