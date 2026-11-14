<#
  Installs the built app to %LOCALAPPDATA%\Programs\Monitor Lamp and adds a
  Start Menu shortcut. The app registers itself to start with Windows
  (Settings → Starting up).

  Usage:  powershell -ExecutionPolicy Bypass -File install.ps1 [-Uninstall]
#>
param([switch]$Uninstall)
$ErrorActionPreference = 'Stop'

# Wherever cargo builds to (target\, or a target-dir set in .cargo/config.toml)
Push-Location $PSScriptRoot
try { $target = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory }
finally { Pop-Location }
$src = Join-Path $target 'release\monitor-lamp.exe'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\Monitor Lamp'
$exe = Join-Path $dest 'Monitor Lamp.exe'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Monitor Lamp.lnk'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

Get-Process 'Monitor Lamp', 'monitor-lamp' -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

if ($Uninstall) {
    Remove-Item $dest -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item $shortcut -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty $runKey 'Monitor Lamp' -ErrorAction SilentlyContinue
    "Uninstalled. Settings remain in $env:APPDATA\Monitor Lamp"
    return
}

if (-not (Test-Path $src)) { throw "Build first: cargo build --release" }
# Clear out any earlier install
if (Test-Path $dest) { Remove-Item (Join-Path $dest '*') -Recurse -Force }
New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item $src $exe -Force

$lnk = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
$lnk.TargetPath = $exe
$lnk.WorkingDirectory = $dest
$lnk.Description = 'Control the Yeelight monitor light bar'
$lnk.Save()

Start-Process $exe
"Installed to $dest and started. Start Menu: Monitor Lamp"
