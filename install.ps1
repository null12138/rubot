#Requires -Version 5.1
param([string]$InstallDir = "$env:LOCALAPPDATA\rubot")

$ErrorActionPreference = "Stop"
$Repo = "opener/rubot"
$AssetPattern = "rubot-windows-amd64.zip"

function Write-Step($msg) { Write-Host "`n===> $msg" -ForegroundColor Cyan }

# --- detect arch ---
if (-not [Environment]::Is64BitOperatingSystem) {
    Write-Error "rubot requires a 64-bit OS"
    exit 1
}

# --- find latest release ---
Write-Step "Fetching latest release info..."
$releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ "User-Agent" = "rubot-installer" }
$asset = $releases.assets | Where-Object { $_.name -eq $AssetPattern } | Select-Object -First 1

if (-not $asset) {
    Write-Error "Could not find $AssetPattern in latest release"
    exit 1
}

# --- download ---
$tmpDir = Join-Path $env:TEMP "rubot-install-$(Get-Random)"
New-Item -ItemType Directory -Path $tmpDir | Out-Null

Write-Step "Downloading $($asset.name)..."
$zipPath = Join-Path $tmpDir $asset.name
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zipPath

# --- extract ---
Write-Step "Extracting..."
Expand-Archive -Path $zipPath -DestinationPath $tmpDir -Force

# --- install ---
if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir | Out-Null
}

$exe = Get-ChildItem -Path $tmpDir -Filter "rubot.exe" -Recurse | Select-Object -First 1
if (-not $exe) {
    Write-Error "rubot.exe not found in archive"
    exit 1
}

Copy-Item $exe.FullName -Destination (Join-Path $InstallDir "rubot.exe") -Force

# --- add to PATH ---
$pathParts = $env:PATH -split ";" | Where-Object { $_ -ne $InstallDir }
$newPath = ($pathParts + $InstallDir) -join ";"
[Environment]::SetEnvironmentVariable("PATH", $newPath, "User")
$env:PATH = $newPath

# --- cleanup ---
Remove-Item -Recurse -Force $tmpDir

# --- check prerequisites ---
Write-Step "Checking prerequisites..."
$python = Get-Command "python" -ErrorAction SilentlyContinue
if (-not $python) {
    Write-Host "  WARNING: python not found — code_exec tool requires it" -ForegroundColor Yellow
    Write-Host "  Install: winget install Python.Python.3" -ForegroundColor Yellow
}

Write-Step "Setup complete!"
Write-Host ""
Write-Host "  rubot installed to: $InstallDir\rubot.exe"
Write-Host ""
Write-Host "  Next steps:"
Write-Host "    1. Create .env:  copy .env.example .env  (then edit with your API key)"
Write-Host "    2. Run rubot:    rubot"
Write-Host ""
Write-Host "  NOTE: Restart your terminal for PATH changes to take effect."
