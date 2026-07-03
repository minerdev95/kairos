# KAIROS releases

Prebuilt binaries. These are **not committed to git** (see `.gitignore`) — attach
them to a GitHub Release instead. This folder documents how each is produced.

```
releases/
├── windows/   kairos-<ver>-windows.zip     — GUI desktop app (double-click kairos.exe)
└── linux/     kairos-<ver>-linux-x86_64.tar.gz — CLI/headless (no GUI deps, portable)
```

## Windows (GUI app)

```powershell
# from the repo root, with the release binary already built:
cargo build --release
powershell -File scripts\package-windows.ps1 -NoBuild -Zip
# → releases\windows\kairos-<ver>-windows.zip
```

The Windows build is the full native desktop app (egui). Double-clicking
`kairos.exe` opens the KAIROS window.

## Linux (CLI / headless)

The Linux release is built **CLI-only** (`--no-default-features`) so it has no X11/
GUI build dependencies and runs on any x86-64 Linux — servers, rigs, headless boxes.

```bash
cargo build --release --no-default-features
# stage: the kairos binary + kairos.toml + README + LICENSE-*
tar czf kairos-<ver>-linux-x86_64.tar.gz kairos-<ver>-linux-x86_64/
```

Run it:

```bash
./kairos --help
./kairos detect
./kairos plan
./kairos start --live --yes     # mine for real
./kairos start --serve          # optional local web dashboard
```

A GUI Linux build is possible too — build with default features and the desktop
`-dev` libraries installed (libxkbcommon, libwayland, libgl, xcb…).

The Linux tarball also bundles **`kairos-stats`**, the project owner's fleet
telemetry ingest server (see `dev/OWNER-SETUP.md`). Run it on a machine you
control: `./kairos-stats 8899` → dashboard at `http://<host>:8899/`.

## Verifying

Each archive is self-contained. The `dev/` overlay (developer-fee addresses,
telemetry) is **never** included in a release — it stays private to the owner's
build environment.
