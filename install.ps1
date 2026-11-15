<#
  Installs the built app to %LOCALAPPDATA%\Programs\Detent and adds a Start Menu
  shortcut. The app registers itself to start with Windows (Settings -> Starting up).
  Also removes Monitor Lamp (Detent's earlier name): its exe, shortcut and Run key.
  Its settings are copied to %APPDATA%\Detent by the app on first start and left in place.

  Usage:  powershell -ExecutionPolicy Bypass -File install.ps1 [-Uninstall]
#>
param([switch]$Uninstall)
$ErrorActionPreference = 'Stop'

# Wherever cargo builds to (target\, or a target-dir set in .cargo/config.toml)
Push-Location $PSScriptRoot
try { $target = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory }
finally { Pop-Location }
$src = Join-Path $target 'release\detent.exe'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\Detent'
$exe = Join-Path $dest 'Detent.exe'
$programs = [Environment]::GetFolderPath('Programs')
$shortcut = Join-Path $programs 'Detent.lnk'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

# Monitor Lamp, the earlier name: removed only once Detent is in place
function Remove-MonitorLamp {
    Remove-Item (Join-Path $env:LOCALAPPDATA 'Programs\Monitor Lamp') -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $programs 'Monitor Lamp.lnk') -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty $runKey 'Monitor Lamp' -ErrorAction SilentlyContinue
}

if ($Uninstall) {
    Get-Process 'Detent', 'detent', 'Monitor Lamp', 'monitor-lamp' -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500
    Remove-MonitorLamp
    Remove-Item $dest -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item $shortcut -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty $runKey 'Detent' -ErrorAction SilentlyContinue
    "Uninstalled. Settings remain in $env:APPDATA\Detent"
    return
}

# Check the build before touching anything that works today
if (-not (Test-Path $src)) { throw "Build first: cargo build --release (nothing was changed)" }

Get-Process 'Detent', 'detent', 'Monitor Lamp', 'monitor-lamp' -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

# Clear out any earlier install of Detent, then copy the new exe; stop here (Monitor Lamp untouched) if it fails
if (Test-Path $dest) { Remove-Item (Join-Path $dest '*') -Recurse -Force }
New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item $src $exe -Force
if (-not (Test-Path $exe)) { throw "Couldn't copy to $exe; Monitor Lamp was left in place" }
Remove-MonitorLamp

$lnk = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
$lnk.TargetPath = $exe
$lnk.WorkingDirectory = $dest
$lnk.Description = 'Desk lamp and knob companion'
$lnk.Save()

# Start with Windows straight away (the app keeps this in sync with its setting afterwards)
Set-ItemProperty $runKey 'Detent' "`"$exe`" --hidden"

Start-Process $exe
"Installed to $dest and started. Start Menu: Detent"
