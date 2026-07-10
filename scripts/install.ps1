# VZT Flow Windows installer — EXPERIMENTAL.
#
# Windows support is best-effort: the hold-to-talk hotkey uses
# tauri-plugin-global-shortcut (Ctrl+Shift+Space) instead of macOS's native
# CGEventTap, and this build receives far less real-world testing than the
# macOS one. Expect rough edges.
#
# Installs the .msi/.exe desktop app, the `flow` CLI (to
# %LOCALAPPDATA%\Programs\vzt-flow\bin, added to your User PATH), and the MCP
# server (to %APPDATA%\vzt-flow\mcp, registered with `claude mcp add` if the
# `claude` CLI is on PATH) — mirroring scripts/install.sh's macOS/Linux
# layout. CLI/MCP packaging is x86_64-only for now; on Windows on Arm this
# step is skipped (see docs/USAGE-Windows.md to build from source).
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
#                        one command. One of: none, asr (default —
#                        parakeet-v3 only, required for ASR to work at all),
#                        all (parakeet-v3 then cleanup). parakeet-v3 is
#                        ~456MB download (~640MB on disk); cleanup is
#                        ~1.1GB. Both land in %APPDATA%\vzt-flow\models\.
#                        The cleanup LLM is macOS-only today (no Windows
#                        `clean`/`polish` support — see docs/USAGE-Windows.md
#                        and README's "Windows (experimental)" section), so
#                        `all` warns that the cleanup model is unused on
#                        Windows rather than silently pulling down 1.1GB for
#                        nothing.

param(
    [switch]$InstallYes,
    [string]$InstallModels = "asr"
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

# Generic latest-release asset fetcher, shared by the setup.exe download and
# the flow-CLI-tarball download below — same gh/REST-API fallback logic
# either way, just a different glob pattern.
function Get-LatestAsset {
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
# -- the user can always retry manually. Invokes the just-installed flow.exe
# by absolute path (not bare `flow.exe`, since a brand-new User PATH entry
# doesn't apply to the current process). $FlowExe is $null when CLI/MCP
# install was skipped (arm64 machine — see the CLI/MCP install block above)
# or failed, in which case this just prints the manual command instead.
function Invoke-ModelDownloads {
    param(
        [string]$Models,
        [string]$FlowExe
    )
    if ($Models -eq "none") { return }

    if (-not $FlowExe -or -not (Test-Path $FlowExe)) {
        Write-Host ""
        Write-Warning "-InstallModels '$Models' requested, but no installed flow.exe was found for this script to invoke (CLI/MCP install skipped or failed above -- see docs/USAGE-Windows.md to build the CLI from source). Once you have a flow.exe, run manually:"
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

Get-LatestAsset -DestDir $WorkDir -Pattern $SetupPattern

$Setup = Get-ChildItem -Path $WorkDir -Filter "*-setup.exe" | Select-Object -First 1
if (-not $Setup) {
    throw "download succeeded but no *-setup.exe found in $WorkDir"
}

Write-Host "==> running $($Setup.Name)"
Start-Process -FilePath $Setup.FullName -Wait

# --- flow CLI + MCP server install ------------------------------------------
# Only the CLI can download the Parakeet ASR model, so without this block a
# Windows user has no path to a working install short of building from
# source. release.yml's `windows` job only packages an x86_64 CLI tarball
# today (`windows-arm64` is attempted-only, see release.yml) — on an arm64
# machine, skip straight to the manual-build pointer below.
$FlowExePath = $null
if ($WinArch -eq "arm64") {
    Write-Warning "flow CLI/MCP packaging is x86_64-only for now -- skipping CLI/MCP install on this arm64 machine. Build from source: see docs/USAGE-Windows.md."
} else {
    $CliPattern = "vzt-flow-cli-windows-x86_64.tar.gz"
    try {
        Write-Host "==> downloading flow CLI + MCP server (pattern: $CliPattern)"
        Get-LatestAsset -DestDir $WorkDir -Pattern $CliPattern

        $CliTarball = Get-ChildItem -Path $WorkDir -Filter $CliPattern | Select-Object -First 1
        if (-not $CliTarball) { throw "download succeeded but $CliPattern not found in $WorkDir" }

        # Windows 10 1803+ / Windows 11 ship tar.exe (bsdtar) natively;
        # Expand-Archive does not handle .tar.gz.
        & tar -xzf $CliTarball.FullName -C $WorkDir
        if ($LASTEXITCODE -ne 0) { throw "tar extraction of $($CliTarball.Name) failed (exit $LASTEXITCODE)" }

        $CliSrcDir = Get-ChildItem -Path $WorkDir -Directory -Filter "vzt-flow-cli-*" | Select-Object -First 1
        if (-not $CliSrcDir) { throw "CLI tarball did not contain the expected vzt-flow-cli-* directory" }

        # --- install flow.exe to a PATH dir (per-user, no admin needed) ---
        $CliDestDir = Join-Path $env:LOCALAPPDATA "Programs\vzt-flow\bin"
        New-Item -ItemType Directory -Path $CliDestDir -Force | Out-Null
        Copy-Item (Join-Path $CliSrcDir.FullName "bin\flow.exe") (Join-Path $CliDestDir "flow.exe") -Force
        $FlowExePath = Join-Path $CliDestDir "flow.exe"
        Write-Host "==> installed flow CLI to $FlowExePath"

        # Only the User PATH is touched -- the Machine PATH needs admin and
        # we don't ask for elevation here.
        $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
        if (-not $UserPath) { $UserPath = "" }
        if (($UserPath -split ';' | Where-Object { $_ }) -notcontains $CliDestDir) {
            $NewUserPath = if ($UserPath) { "$UserPath;$CliDestDir" } else { $CliDestDir }
            [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
            Write-Host "==> added $CliDestDir to your User PATH -- this only applies to NEW shells; restart your terminal to use bare 'flow'."
        }

        # --- install the MCP server (mirrors install.sh's ~/.vzt-flow/mcp layout) ---
        $McpDest = Join-Path $env:APPDATA "vzt-flow\mcp"
        Write-Host "==> installing MCP server to $McpDest"
        if (Test-Path $McpDest) { Remove-Item -Recurse -Force $McpDest }
        New-Item -ItemType Directory -Path $McpDest -Force | Out-Null
        # Flatten dist/'s contents directly into $McpDest so the entry point
        # is $McpDest\index.js, matching the `claude mcp add` path below.
        Copy-Item (Join-Path $CliSrcDir.FullName "mcp\dist\*") $McpDest -Recurse -Force
        $McpNodeModules = Join-Path $CliSrcDir.FullName "mcp\node_modules"
        if (Test-Path $McpNodeModules) {
            Copy-Item $McpNodeModules (Join-Path $McpDest "node_modules") -Recurse -Force
        }
        $McpPackageJson = Join-Path $CliSrcDir.FullName "mcp\package.json"
        if (Test-Path $McpPackageJson) {
            Copy-Item $McpPackageJson (Join-Path $McpDest "package.json") -Force
        }

        $McpEntry = Join-Path $McpDest "index.js"
        $claude = Get-Command claude -ErrorAction SilentlyContinue
        if ((Test-Path $McpEntry) -and $claude) {
            Write-Host "==> registering vzt-flow with claude mcp"
            & claude mcp remove vzt-flow --scope user 2>$null | Out-Null
            & claude mcp add vzt-flow --scope user -- node "$McpEntry"
            if ($LASTEXITCODE -ne 0) {
                Write-Warning "failed to register MCP server via 'claude mcp add' -- retry manually: claude mcp add vzt-flow --scope user -- node `"$McpEntry`""
            }
        } else {
            if (-not (Test-Path $McpEntry)) { Write-Warning "MCP entry file not found at $McpEntry -- skipping claude mcp registration" }
            if (-not $claude) { Write-Host "==> claude CLI not found -- skipping MCP registration (install later with: claude mcp add vzt-flow --scope user -- node `"$McpEntry`")" }
        }
    } catch {
        Write-Warning "flow CLI/MCP install failed ($_) -- you can retry manually from the latest GitHub release, or build from source (see docs/USAGE-Windows.md)."
        $FlowExePath = $null
    }
}

Write-Host ""
Write-Host "VZT Flow installed (experimental Windows build)." -ForegroundColor Green
Write-Host "Hold-to-talk hotkey: Ctrl+Shift+Space (global shortcut, registered on launch)."
Write-Host "If the hotkey doesn't register, another app may already be using it."
Write-Host ""
if ($FlowExePath) {
    Write-Host "CLI:  flow --help    (open a NEW terminal first -- PATH was just updated)"
    Write-Host "MCP:  claude mcp list   (should show `"vzt-flow`")"
} else {
    Write-Host "CLI/MCP install was skipped or failed above -- see docs/USAGE-Windows.md"
    Write-Host "to build the CLI from source, or the macOS installer (scripts/install.sh)"
    Write-Host "for the equivalent flow CLI + MCP server setup on Mac."
}

Invoke-ModelDownloads -Models $InstallModels -FlowExe $FlowExePath

Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue
