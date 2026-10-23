# Recall installer for Windows (PowerShell 5.1+).
#
#   irm https://github.com/j4ckxyz/recall/releases/latest/download/install.ps1 | iex
#
# Environment:
#   RECALL_VERSION       release tag to install (default: latest)
#   RECALL_INSTALL_DIR   where to put recall.exe (default: %LOCALAPPDATA%\Programs\recall)
#   RECALL_CONNECT       yes | no - add Recall to detected agents without asking
#   RECALL_ARTIFACT_DIR  install from local release files instead of downloading (CI)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'j4ckxyz/recall'
$Version = if ($env:RECALL_VERSION) { $env:RECALL_VERSION } else { 'latest' }
$InstallDir = if ($env:RECALL_INSTALL_DIR) { $env:RECALL_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\recall' }

function Step($msg) { Write-Host "`n==> $msg" -ForegroundColor Cyan }
# throw, not exit: `exit` would close the user's window under `irm | iex`.
function Die($msg) { throw "recall install: $msg" }

# x64 build; Windows on ARM runs it under emulation.
$Target = 'x86_64-pc-windows-msvc'
$Asset = "recall-$Target.zip"
$Tmp = Join-Path ([IO.Path]::GetTempPath()) ("recall-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $Tmp | Out-Null

try {
    Step "Downloading Recall ($Version) for $Target"
    if ($env:RECALL_ARTIFACT_DIR) {
        Copy-Item (Join-Path $env:RECALL_ARTIFACT_DIR $Asset), (Join-Path $env:RECALL_ARTIFACT_DIR 'SHA256SUMS') $Tmp
    } else {
        $Base = if ($Version -eq 'latest') { "https://github.com/$Repo/releases/latest/download" } else { "https://github.com/$Repo/releases/download/$Version" }
        Invoke-WebRequest -UseBasicParsing "$Base/$Asset" -OutFile (Join-Path $Tmp $Asset)
        Invoke-WebRequest -UseBasicParsing "$Base/SHA256SUMS" -OutFile (Join-Path $Tmp 'SHA256SUMS')
    }

    $line = Get-Content (Join-Path $Tmp 'SHA256SUMS') | Where-Object { $_ -match " $([regex]::Escape($Asset))$" } | Select-Object -First 1
    if (-not $line) { Die "$Asset is not listed in SHA256SUMS" }
    $Expected = ($line -split '\s+')[0].ToLower()
    $Actual = (Get-FileHash -Algorithm SHA256 (Join-Path $Tmp $Asset)).Hash.ToLower()
    if ($Expected -ne $Actual) { Die "checksum mismatch for $Asset (expected $Expected, got $Actual)" }
    Write-Host "Checksum verified (sha256 $Actual)"

    Step "Installing to $InstallDir"
    Expand-Archive -Force (Join-Path $Tmp $Asset) $Tmp
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $Bin = Join-Path $InstallDir 'recall.exe'
    # A running recall.exe can't be overwritten, but it can be renamed aside.
    if (Test-Path $Bin) { Move-Item -Force $Bin "$Bin.old" }
    Copy-Item (Join-Path $Tmp 'recall.exe') $Bin
    Remove-Item -Force "$Bin.old" -ErrorAction SilentlyContinue
    & $Bin --version
    if ($LASTEXITCODE -ne 0) { Die 'the installed binary does not run on this system' }

    $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($UserPath -split ';') -contains $InstallDir)) {
        [Environment]::SetEnvironmentVariable('Path', ($UserPath.TrimEnd(';') + ";$InstallDir").TrimStart(';'), 'User')
        Write-Host "Added $InstallDir to your user PATH (open a new terminal to use it)."
    }
    $env:Path = "$env:Path;$InstallDir"

    Step 'Creating your encrypted memory store'
    & $Bin init
    if ($LASTEXITCODE -ne 0) { Die 'recall init failed' }

    Step 'Self-test'
    & $Bin doctor
    if ($LASTEXITCODE -ne 0) { Die "self-test failed; please open an issue: https://github.com/$Repo/issues" }

    $Connect = if ($env:RECALL_CONNECT) { $env:RECALL_CONNECT } else { 'ask' }
    if ($Connect -eq 'ask') {
        if ([Environment]::UserInteractive -and -not [Console]::IsInputRedirected) {
            $reply = Read-Host "`nAdd Recall to the agents found above? [Y/n]"
            $Connect = if ($reply -match '^[nN]') { 'no' } else { 'yes' }
        } else { $Connect = 'no' }
    }
    if ($Connect -eq 'yes') {
        Step 'Connecting agents'
        & $Bin connect
    }

    Step 'Recall is installed'
    Write-Host "Key: $env:USERPROFILE\.recall\key - back it up; without it your memory cannot be decrypted."
    if ($Connect -ne 'yes') { Write-Host 'Connect your agents any time with: recall connect' }
    Write-Host "Share one memory across devices with Tailscale: https://github.com/$Repo#use-it-across-devices-with-tailscale"
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
