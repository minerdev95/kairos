# KAIROS

**A smarter crypto miner.** KAIROS mines through your pools with its *own* built‑in
engine and adds a brain on top that decides **what to mine, when to switch, when to
stop, and how much power to use** — so you earn more profit per watt, not just more
hashes.

Think of it as an alternative to Awesome Miner or HiveOS, but with two differences:

1. **It's its own miner.** KAIROS talks to pools and computes the proof‑of‑work
   itself. It does **not** launch T‑Rex, lolMiner, XMRig, or any other third‑party
   miner.
2. **It optimizes for money, not megahashes.** It watches live prices and difficulty,
   picks the most profitable coin for each device, and tunes each GPU's power limit to
   the point where you keep the most dollars after your electricity bill.

It runs as a **desktop app** (just double‑click) or from the **command line**, on
**Windows and Linux**.

---

## Download & run

Grab the latest build from the [**Releases**](../../releases) page.

**Windows**
1. Download `kairos-0.1.0-windows.zip` and unzip it.
2. Double‑click `kairos.exe` — the app opens.
3. (Optional) run `install.ps1` to add Start‑menu / desktop shortcuts.

**Linux**
```bash
tar -xzf kairos-0.1.0-linux-x86_64.tar.gz
cd kairos-0.1.0-linux-x86_64
./install.sh          # optional: copies kairos to /usr/local/bin
./kairos help
```
(The Linux build is command‑line only. Needs glibc 2.31+ — Ubuntu 20.04 / Debian 11 or
newer.)

Nothing connects or mines until **you** say so. The default is a safe **preview** mode
that shows exactly what KAIROS *would* mine and why. To actually mine, you add the
`--yes` flag (CLI) or press **Start** in the app.

---

## What you can mine

| Coins | How | Status |
|---|---|---|
| **BTC · BCH · DGB** (SHA‑256) | KAIROS's own engine | ✅ Works today |
| **LTC · DOGE · DGB** (scrypt) | KAIROS's own engine | ✅ Works today |
| **KAS** (Kaspa / kHeavyHash) | KAIROS's own engine | ⚠️ Experimental — verify first (below) |
| ERG · ETC · RVN · XMR | — | 🔜 On the roadmap |

You can point KAIROS at **any pool you like** — add the URL, your wallet, and worker
name in Settings, and it handles the rest.

> **Note on hardware:** SHA‑256 and scrypt coins are dominated by ASICs, so mining them
> on a CPU is for testing, not profit. The real GPU money‑maker is Kaspa — see below.

---

## Verify a Kaspa pool before you mine

Kaspa uses a different pool protocol, so KAIROS lets you **test a pool in one command**
before committing any hash power. It does the full handshake and shows you what it read
back — it does **not** submit any shares:

```bash
kairos kaspa-verify stratum+tcp://eu1.kaspa-pool.org:4444 kaspa:YOUR_ADDRESS.worker1
```

If it prints a job with a prePowHash, timestamp, and target, KAIROS understands your
pool. (Kaspa stays labelled *experimental* until you've confirmed a few **accepted**
shares while actually mining — please tell us if a pool rejects.)

For any other pool, `kairos poolcheck <url>` tells you whether KAIROS can mine it.

---

## Manage your ASICs

If you run ASIC miners (Antminer, Whatsminer, Avalon, Goldshell, …), KAIROS can see and
control them over their standard network API — no extra software on the miner:

```bash
kairos asic scan 192.168.1.0/24     # find ASICs on your network
kairos asic status 192.168.1.50     # hashrate, temps, shares, pools
kairos asic switch 192.168.1.50 stratum+tcp://pool:3333 wallet.worker --yes
```

---

## Save money on electricity

KAIROS doesn't just chase the highest hashrate — it finds the **power limit that makes
the most profit** for each GPU. Because the last chunk of a card's power usually buys
only a tiny bit more speed, running a little lower often earns **more** once your
electricity bill is counted. Set your power price in **Settings → Economics** (or
`kairos.toml`), and KAIROS does the math per coin, per card, live.

```toml
[economics]
power_cost_usd_kwh = 0.10   # your electricity price
auto_power_limit   = true   # tune each GPU for max profit-per-watt
min_profit_usd_day = 0.0    # only mine above this profit floor
```

---

## Handy commands

```
kairos                 open the desktop app (same as double-click)
kairos detect          show your CPUs / GPUs
kairos plan            what KAIROS would mine right now, and why
kairos profit          best coin to mine, ranked by $/day
kairos engine          which algorithms KAIROS can hash
kairos hashbench       measure your machine's hashrate
kairos start --serve   open the live web dashboard in your browser
kairos help            full command list
```

---

## The 1% developer fee

KAIROS carries a disclosed **1% developer fee** that supports ongoing development: for a
small slice of mining time it mines to the developer's wallet. It's transparent, it's
disclosed here, and it never touches your wallet keys — your payouts go straight to
**your** addresses from your pools.

## Privacy

By default KAIROS sends **nothing** anywhere. A build *may* be configured to report
**anonymous** usage stats (a random ID, version, OS, and which coins/pools are mined —
**never** wallet addresses or personal data), and only to a server the project owner
runs. It's off unless explicitly enabled.

## Safety

KAIROS never asks for or stores a wallet **private key**, and it never moves your funds.
It won't connect or mine without your explicit `--yes` / **Start**. Actions that could
cost money (like repointing an ASIC's pool) always require confirmation.

---

## License

**Proprietary — all rights reserved.** See [LICENSE](LICENSE). You may run official,
unmodified builds of KAIROS. Copying, redistributing, modifying, forking, reverse‑
engineering, or removing/bypassing the developer fee are not permitted. For commercial
or partnership use, please get in touch.

---

*KAIROS is under active development. Bitcoin‑family mining (SHA‑256d / scrypt) is
verified end‑to‑end; Kaspa is implemented and needs live‑pool confirmation; more GPU
algorithms are on the way. Mining is done at your own risk.*
