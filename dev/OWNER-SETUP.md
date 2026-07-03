# KAIROS — owner setup (private)

This file (and everything else that reads `dev/dev.toml`) is for the **project
owner**. `dev/dev.toml` is gitignored; only the template `dev.toml.example` is
tracked. None of this ships in a public/open build.

## 1. Arm the developer fee (per-coin, tamper-resistant)

The disclosed 1% routes to *your* per-coin payout addresses. It collects for a
coin only when that coin has a real address.

```bash
cp dev/dev.toml.example dev/dev.toml
kairos dev-hash "your-strong-admin-passphrase"     # prints a SHA-256
# paste that into admin_key_sha256, then fill [wallets] with YOUR payout addresses
```

Then **rebuild** — `build.rs` embeds a filled `dev/dev.toml` into the binary, so
the shipped release carries your addresses compiled in. A user editing their local
`dev.toml` cannot redirect the fee; only a rebuild can (baked config wins over any
runtime file). *(Client-side fees are never 100% un-bypassable — a determined user
can patch a binary — but baking matches how commercial miners protect a disclosed
fee.)*

```bash
cargo build --release          # Windows GUI build, bakes dev/dev.toml
```

An untouched template (still containing `PASTE_OUTPUT`) bakes nothing, so a
demo/public build stays clean.

Never put a private key here. These are public payout addresses only.

### ⚠ Do NOT use exchange deposit addresses as dev-fee payout addresses

The dev fee is paid by **the pools**, on their own schedule, to whatever address is
baked in. If that's an **exchange deposit address**, then as KAIROS spreads across
many miners/pools you get **many small deposits in a short time** — the classic
"dust" pattern that exchanges flag and can **freeze or ban** the account for.

Mitigations, in order of importance:

1. **Use a self-custody wallet** (your own wallet/node) as the baked payout address,
   **not** an exchange address. Periodically consolidate and send **one larger**
   transfer to the exchange yourself. This is the only reliable fix.
2. **Raise each pool's minimum payout** for the payout address (set high in the pool
   dashboard) so payouts are fewer and larger. Set `min_payout` per coin in
   `dev/dev.toml` as a reminder/marker of your intended threshold:

   ```toml
   [payout]
   # informational: the min-payout you've configured at each pool, per coin
   KAS = 100     # only let a KAS pool pay out at ≥100 KAS
   LTC = 1.0
   ```

3. Prefer **PPLNS pools with high thresholds** over frequent-payout PPS pools for the
   dev address.

`kairos dev-check` prints a reminder about this whenever a payout address is baked.

## 2. Fleet telemetry — see who runs KAIROS

Disclosed, opt-in (README carries the privacy notice). Run the ingest server on a
machine you control, then set the endpoint in `dev/dev.toml` and rebuild.

```bash
# build the server (part of the crate):
cargo build --release --bin kairos-stats           # → target/release/kairos-stats
# run it on your VPS (default port 8899):
./kairos-stats 8899
#   dashboard → http://<your-host>:8899/
#   ingest    → http://<your-host>:8899/ingest
```

Point the miner at it in `dev/dev.toml`:

```toml
[telemetry]
enabled = true
endpoint = "http://<your-host>:8899/ingest"
interval_secs = 300
```

Each miner then POSTs **anonymous** snapshots only — a random instance id, version,
OS, coins/pools mined, and hashrate. No wallet addresses, no personal data. The
dashboard shows active miners, coins, pools, OS, versions, and fleet hashrate; raw
events are appended to `kairos-stats.jsonl` on the server.

## 3. Deploy the server (systemd example)

```ini
# /etc/systemd/system/kairos-stats.service
[Unit]
Description=KAIROS telemetry
After=network.target
[Service]
ExecStart=/root/kairos-stats 8899
WorkingDirectory=/root
Restart=always
[Install]
WantedBy=multi-user.target
```

```bash
systemctl enable --now kairos-stats
```

Put it behind TLS (a reverse proxy) and use an `https://` endpoint for production.
