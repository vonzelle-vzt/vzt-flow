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
#   -InstallYes          never prompt
#   $env:GITHUB_TOKEN     optional; only needed as a fallback if `gh` isn't
#                        installed and unauthenticated GitHub API requests
#                        are rate-limited (60/hr per IP), or for a private fork
#   -InstallModels /      opt-in, non-interactive model download so an
#   $env:INSTALL_MODELS   agent/CI can complete a full end-to-end install in
#                        one command. One of: none (default — matches prior
#                        behavior), asr (parakeet-v3 only), all (parakeet-v3
#                        then cleanup). parakeet-v3 is ~456MB download
#                        (~640MB on disk); cleanup is ~1.1GB. Both would land
#                        in %USERPROFILE%\.config\vzt-flow\models\. NOTE:
#                        CLI/MCP packaging is not yet available for Windows
#                        (see below) — there is no installed flow.exe for
#                        this script to invoke yet, so asr/all currently
#                        print instructions instead of downloading. Also,
#                        the cleanup LLM is macOS-only today (no Windows
#                        `clean`/`polish` support — see docs/USAGE-Windows.md
#                        and README's "Windows (experimental)" section), so
#                        even once CLI packaging lands, `all` would warn that
#                        the cleanup model is unused on Windows rather than
#                        silently pulling down 1.1GB for nothing.

param(
    [switch]$InstallYes,
    [string]$InstallModels = "none"
)

if (-not $PSBoundParameters.ContainsKey('InstallModels') -and $env:INSTALL_MODELS) {
    $InstallModels = $env:INSTALL_MODELS
}

# Validate early (before any download/install work starts) so a typo fails
# fast instead of partway through the install.
if ($InstallModels -notin @("none", "asr", "all")) {
    Write-Error "invalid -InstallModels/env:INSTALL_MODELS '$InstallModels' (valid values: none, asr, all)"
    exit 1
}

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

# Tauri names the NSIS installer "{productName}_{version}_{arch}-setup.exe",
# where {arch} is "x64" or "arm64" (see tauri-bundler's nsis/mod.rs) — pick
# the one matching this machine so an arm64 box doesn't silently install (or
# fail to find) the wrong installer once both are shipped.
$WinArch = if ([System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture -eq [System.Runtime.InteropServices.Architecture]::Arm64) { "arm64" } else { "x64" }
if ($WinArch -eq "arm64") {
    Write-Host "==> Windows on Arm detected — this build is attempted-only in CI (may not exist for every release)" -ForegroundColor Yellow
}
$SetupPattern = "*_${WinArch}-setup.exe"

function Get-LatestSetupExe {
    param([string]$DestDir, [string]$Pattern)

    $gh = Get-Command gh -ErrorAction SilentlyContinue
    if ($gh) {
        Write-Host "==> downloading via gh release download (pattern: $Pattern)"
        & gh release download --repo $Repo --pattern $Pattern --dir $DestDir --clobber
        if ($LASTEXITCODE -ne 0) { throw "gh release download failed (no asset matching '$Pattern'? arm64 builds are attempted-only in CI)" }
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
    $regex = [regex]::Escape($Pattern) -replace '\\\*', '.*'
    $asset = $release.assets | Where-Object { $_.name -match "^$regex$" } | Select-Object -First 1
    if (-not $asset) { throw "no asset matching '$Pattern' found on latest release (arm64 builds are attempted-only in CI)" }

    $dlHeaders = @{ Accept = "application/octet-stream" }
    if ($token) { $dlHeaders["Authorization"] = "Bearer $token" }
    $outFile = Join-Path $DestDir $asset.name
    Invoke-WebRequest -Uri $asset.url -Headers $dlHeaders -OutFile $outFile
}

# --- opt-in model download (-InstallModels asr|all) ------------------------
# A failed download warns and does not abort an otherwise-successful install
# -- the user can always retry manually. Would invoke the just-installed
# flow.exe by absolute path (not bare `flow.exe`, since it may not be on
# PATH) -- but see the header note: CLI/MCP packaging isn't published for
# Windows yet, so there is no flow.exe for this script to invoke today.
function Invoke-ModelDownloads {
    param(
        [string]$Models,
        [string]$FlowExe
    )
    if ($Models -eq "none") { return }

    if (-not $FlowExe -or -not (Test-Path $FlowExe)) {
        Write-Host ""
        Write-Warning "-InstallModels '$Models' requested, but CLI/MCP packaging is not yet available for Windows -- there is no installed flow.exe for this script to invoke (see docs/USAGE-Windows.md to build the CLI from source). Once built, run manually:"
        Write-Host "  flow.exe models download parakeet-v3"
        if ($Models -eq "all") {
            Write-Host "  flow.exe models download cleanup   # optional -- the cleanup LLM is macOS-only today (no Windows clean/polish support yet), so this model isn't used on Windows"
        }
        return
    }

    try {
        Write-Host "==> downloading ASR model (parakeet-v3, ~456MB download / ~640MB on disk)"
        & $FlowExe models download parakeet-v3
        if ($LASTEXITCODE -ne 0) { throw "exit code $LASTEXITCODE" }
    } catch {
        Write-Warning "model download failed for parakeet-v3 ($_) -- retry with: $FlowExe models download parakeet-v3"
    }

    if ($Models -eq "all") {
        Write-Warning "the cleanup LLM is macOS-only today (no Windows clean/polish support yet) -- downloading it anyway per -InstallModels all, but it won't be used on Windows."
        try {
            Write-Host "==> downloading cleanup model (cleanup, ~1.1GB)"
            & $FlowExe models download cleanup
            if ($LASTEXITCODE -ne 0) { throw "exit code $LASTEXITCODE" }
        } catch {
            Write-Warning "model download failed for cleanup ($_) -- retry with: $FlowExe models download cleanup"
        }
    }
}

Get-LatestSetupExe -DestDir $WorkDir -Pattern $SetupPattern

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

Invoke-ModelDownloads -Models $InstallModels -FlowExe $null

Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue
