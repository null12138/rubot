#Requires -Version 5.1
param(
    [ValidateSet("install", "update", "uninstall")]
    [string]$Action = "",
    [string]$InstallDir = ""
)

$ErrorActionPreference = "Stop"
$Repo = "null12138/rubot"
$AssetName = "rubot-windows-amd64.zip"
$DownloadUrl = "https://github.com/$Repo/releases/latest/download/$AssetName"

if ([string]::IsNullOrWhiteSpace($Action)) {
    $Action = if ($env:RUBOT_INSTALL_ACTION) { $env:RUBOT_INSTALL_ACTION } else { "install" }
}
if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = if ($env:RUBOT_INSTALL_DIR) { $env:RUBOT_INSTALL_DIR } else { "$env:LOCALAPPDATA\rubot\bin" }
}

function Write-Step($Message) {
    Write-Host "`n===> $Message" -ForegroundColor Cyan
}

function Update-UserPath($Dir, [bool]$Remove) {
    $current = [Environment]::GetEnvironmentVariable("PATH", "User")
    $parts = @()
    if ($current) {
        $parts = $current -split ";" | Where-Object { $_ -and $_ -ne $Dir }
    }
    if (-not $Remove) {
        $parts += $Dir
    }
    $newPath = ($parts | Select-Object -Unique) -join ";"
    [Environment]::SetEnvironmentVariable("PATH", $newPath, "User")
    $env:PATH = $newPath
}

if ($Action -eq "uninstall") {
    $exePath = Join-Path $InstallDir "rubot.exe"
    if (Test-Path $exePath) {
        Remove-Item -Force $exePath
        Write-Step "Removed $exePath"
    } else {
        Write-Warning "No installed rubot found at $exePath"
    }

    Update-UserPath -Dir $InstallDir -Remove $true

    if ((Test-Path $InstallDir) -and -not (Get-ChildItem -Force $InstallDir | Select-Object -First 1)) {
        Remove-Item -Force $InstallDir
    }

    Write-Host "rubot uninstall complete."
    exit 0
}

if (-not [Environment]::Is64BitOperatingSystem) {
    Write-Error "rubot requires a 64-bit OS"
    exit 1
}

Write-Step "Preparing installer for $AssetName"

$tmpDir = Join-Path $env:TEMP "rubot-install-$(Get-Random)"
New-Item -ItemType Directory -Path $tmpDir | Out-Null

try {
    Write-Step "Downloading $AssetName..."
    $zipPath = Join-Path $tmpDir $AssetName
    Invoke-WebRequest -Uri $DownloadUrl -Headers @{ "User-Agent" = "rubot-installer" } -OutFile $zipPath

    Write-Step "Extracting..."
    Expand-Archive -Path $zipPath -DestinationPath $tmpDir -Force

    $exe = Get-ChildItem -Path $tmpDir -Filter "rubot.exe" -Recurse | Select-Object -First 1
    if (-not $exe) {
        Write-Error "rubot.exe not found in archive"
        exit 1
    }

    if (-not (Test-Path $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir | Out-Null
    }

    $Verb = if ($Action -eq "update") { "Updating" } else { "Installing" }
    Write-Step "$Verb to $InstallDir"
    Copy-Item $exe.FullName -Destination (Join-Path $InstallDir "rubot.exe") -Force

    Update-UserPath -Dir $InstallDir -Remove $false

    $versionOutput = & (Join-Path $InstallDir "rubot.exe") --version 2>$null
    if ($versionOutput) {
        Write-Step "Installed $versionOutput"
    }

    $python = Get-Command "python" -ErrorAction SilentlyContinue
    if (-not $python) {
        Write-Host "WARNING: python not found — code_exec may need it" -ForegroundColor Yellow
    }

    Write-Host ""
    Write-Step "Done."
    Write-Host "  Run: rubot --version"
    Write-Host "  Start: rubot"
    Write-Host "  Configure inside rubot with:"
    Write-Host "    /config set api_base_url <url>"
    Write-Host "    /config set api_key <key>"
    Write-Host "    /config set model <model>"
    Write-Host ""
    Write-Host "If PATH changed, restart the terminal."
}
finally {
    if (Test-Path $tmpDir) {
        Remove-Item -Recurse -Force $tmpDir
    }
}
