# VZT Flow Windows installer — EXPERIMENTAL.
#
# Windows support is best-effort: the hold-to-talk hotkey uses
# tauri-plugin-global-shortcut (Ctrl+Shift+Space) instead of macOS's native
# CGEventTap, and this build receives far less real-world testing than the
# macOS one. Expect rough edges.
#
#   iwr https://raw.githubusercontent.com/vonzelle-vzt/vzt-flow/main/scripts/install.ps1 -UseBasicParsing | iex
#
# The repo is public, so no authentication is needed for the default path.
#
# Env / params:
#   -InstallYes         never prompt
#   $env:GITHUB_TOKEN    optional; only needed as a fallback if `gh` isn't
#                        installed and unauthenticated GitHub API requests
#                        are rate-limited (60/hr per IP), or for a private fork

param(
    [switch]$InstallYes
)

$ErrorActionPreference = "Stop"
$Repo = "vonzelle-vzt/vzt-flow"

Write-Host "==> VZT Flow Windows installer (EXPERIMENTAL)" -ForegroundColor Yellow
Write-Host "    Windows support is newer and less battle-tested than macOS." -ForegroundColor Yellow

if (-not $InstallYes -and -not $env:INSTALL_YES) {
    $reply = Read-Host "Continue? [y/N]"
    if ($reply -notmatch '^[yY]') {
        Write-Host "install aborted"
        exit 1
    }
}

$WorkDir = Join-Path $env:TEMP ("vzt-flow-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $WorkDir | Out-Null

function Get-LatestSetupExe {
    param([string]$DestDir)

    $gh = Get-Command gh -ErrorAction SilentlyContinue
    if ($gh) {
        Write-Host "==> downloading via gh release download"
        & gh release download --repo $Repo --pattern "*-setup.exe" --dir $DestDir --clobber
        if ($LASTEXITCODE -ne 0) { throw "gh release download failed" }
        return
    }

    # Repo is public — GITHUB_TOKEN is optional. Set it to lift GitHub's
    # 60/hr-per-IP unauthenticated API rate limit, or to fetch from a
    # private fork.
    $token = $env:GITHUB_TOKEN
    Write-Host "==> downloading via GitHub REST API$(if ($token) { ' (authenticated)' })"
    $headers = @{ Accept = "application/vnd.github+json" }
    if ($token) { $headers["Authorization"] = "Bearer $token" }
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
    $asset = $release.assets | Where-Object { $_.name -like "*-setup.exe" } | Select-Object -First 1
    if (-not $asset) { throw "no *-setup.exe asset found on latest release" }

    $dlHeaders = @{ Accept = "application/octet-stream" }
    if ($token) { $dlHeaders["Authorization"] = "Bearer $token" }
    $outFile = Join-Path $DestDir $asset.name
    Invoke-WebRequest -Uri $asset.url -Headers $dlHeaders -OutFile $outFile
}

Get-LatestSetupExe -DestDir $WorkDir

$Setup = Get-ChildItem -Path $WorkDir -Filter "*-setup.exe" | Select-Object -First 1
if (-not $Setup) {
    throw "download succeeded but no *-setup.exe found in $WorkDir"
}

Write-Host "==> running $($Setup.Name)"
Start-Process -FilePath $Setup.FullName -Wait

Write-Host ""
Write-Host "VZT Flow installed (experimental Windows build)." -ForegroundColor Green
Write-Host "Hold-to-talk hotkey: Ctrl+Shift+Space (global shortcut, registered on launch)."
Write-Host "If the hotkey doesn't register, another app may already be using it."
Write-Host ""
Write-Host "CLI/MCP packaging is not yet available for Windows — see the macOS"
Write-Host "installer (scripts/install.sh) for the flow CLI + MCP server setup on Mac."

Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue
