<#
  KAIROS — Windows installer.
  Builds the release binary and creates branded Start-Menu and Desktop shortcuts
  that launch the control plane and open its dashboard. Run from the repo root:

      powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1

  Flags:
      -NoBuild     skip cargo build (use the existing target\release\kairos.exe)
      -NoDesktop   do not create a Desktop shortcut
#>
param(
  [switch]$NoBuild,
  [switch]$NoDesktop
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe  = Join-Path $root "target\release\kairos.exe"
$ico  = Join-Path $root "assets\kairos.ico"

if (-not $NoBuild) {
  Write-Host "Building KAIROS (release)..." -ForegroundColor Cyan
  Push-Location $root
  cargo build --release
  Pop-Location
}
if (-not (Test-Path $exe)) { throw "kairos.exe not found at $exe — build first." }

function New-KairosShortcut([string]$linkPath) {
  $shell = New-Object -ComObject WScript.Shell
  $sc = $shell.CreateShortcut($linkPath)
  $sc.TargetPath       = $exe
  $sc.Arguments        = "start --serve"
  $sc.WorkingDirectory = $root
  $sc.Description       = "KAIROS — intelligent mining control plane"
  if (Test-Path $ico) { $sc.IconLocation = "$ico,0" } else { $sc.IconLocation = "$exe,0" }
  $sc.Save()
  Write-Host "  created $linkPath" -ForegroundColor Green
}

$startMenu = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\KAIROS.lnk"
New-KairosShortcut $startMenu
if (-not $NoDesktop) {
  New-KairosShortcut (Join-Path ([Environment]::GetFolderPath("Desktop")) "KAIROS.lnk")
}

Write-Host ""
Write-Host "KAIROS installed. Launch from the Start Menu or run:" -ForegroundColor Cyan
Write-Host "  $exe start --serve" -ForegroundColor White
Write-Host "Dashboard: http://127.0.0.1:4280/dash" -ForegroundColor White
