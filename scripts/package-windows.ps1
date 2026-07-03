<#
  KAIROS - Windows release packager.
  Builds the release binary and assembles a self-contained, zippable folder:

      dist\kairos-<version>-windows\
        kairos.exe          (icon embedded; dashboard is compiled in)
        kairos.toml         (sample config - pools/wallets/alerts/...)
        README.md
        install.ps1         (creates branded Start-Menu / Desktop shortcuts)
        assets\kairos.ico

  Run from the repo root:
      powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1 -Zip
#>
param([string]$Version = "0.1.0", [switch]$NoBuild, [switch]$Zip)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

if (-not $NoBuild) {
  Write-Host "Building KAIROS (release)..." -ForegroundColor Cyan
  Push-Location $root; cargo build --release; Pop-Location
}

$exe = Join-Path $root "target\release\kairos.exe"
if (-not (Test-Path $exe)) { throw "kairos.exe not found - build first." }

$name = "kairos-$Version-windows"
$relDir = Join-Path $root "releases\windows"
New-Item -ItemType Directory -Force -Path $relDir | Out-Null
$out  = Join-Path $relDir $name
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Force -Path (Join-Path $out "assets") | Out-Null

Copy-Item $exe (Join-Path $out "kairos.exe")
Copy-Item (Join-Path $root "kairos.toml") (Join-Path $out "kairos.toml")
Copy-Item (Join-Path $root "README.md")   (Join-Path $out "README.md")
Copy-Item (Join-Path $root "LICENSE") (Join-Path $out "LICENSE")
Copy-Item (Join-Path $root "assets\kairos.ico") (Join-Path $out "assets\kairos.ico")
Copy-Item (Join-Path $root "scripts\install-windows.ps1") (Join-Path $out "install.ps1")

$readme = @'
KAIROS - intelligent mining control plane (Windows)

  KAIROS mines through your pools with its OWN native engine - its own Stratum
  client and its own proof-of-work kernels (SHA-256d, kHeavyHash) - no third-
  party miner binaries. An intelligent brain decides what to mine, when to
  switch, and when to idle, for more profit than a fixed setup.

  QUICK START
  1) Double-click kairos.exe -> the native KAIROS app window opens (not a browser).
  2) Go to the Settings tab. Enter your payout wallets (any coin) and your pool
     connections (URL, worker, pass, scheme, priority). Click Save. No file editing.
  3) On the Mining tab, press "Start mining" to connect KAIROS's own engine to your
     pool and begin hashing. Press Stop any time. Nothing hashes until you press Start.
  4) The Engine tab shows the native kernels and benchmarks your CPU's hashrate.

  GPU hashing uses KAIROS's own CUDA kernels - build with  --features gpu
  (needs the CUDA toolkit). The CPU engine works out of the box.

  COMMAND LINE (optional, for scripting / headless)
        kairos.exe detect        your real GPUs / CPU + capability
        kairos.exe engine        the native hashing backends + kernel per algo
        kairos.exe hashbench     measure the native engine's hashrate on your CPU
        kairos.exe plan          live prices -> what it would mine, and the pool
        kairos.exe start --live --yes   mine for real from the terminal
        kairos.exe start --serve        the optional web dashboard instead of the app
        kairos.exe --help        all commands

  The built-in digital twin (no --live) is the safe validation/sim mode -
  it runs the exact same brain with no hardware required.

  Optional: run  install.ps1  to add Start-Menu / Desktop shortcuts.
'@
Set-Content -Encoding UTF8 -Path (Join-Path $out "READ-ME-FIRST.txt") -Value $readme

Write-Host "Packaged -> $out" -ForegroundColor Green
Get-ChildItem $out -Recurse -File | ForEach-Object { Write-Host ("  " + $_.FullName.Substring($out.Length+1)) }

if ($Zip) {
  # NB: $zip would alias the [switch]$Zip parameter (case-insensitive) and fail
  # the String assignment, so use a distinct name.
  $zipPath = Join-Path $relDir "$name.zip"
  if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
  Compress-Archive -Path $out -DestinationPath $zipPath
  Write-Host "Zipped -> $zipPath" -ForegroundColor Green
}
