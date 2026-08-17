<#
.SYNOPSIS
  treehouse installer for Windows (PowerShell).

.DESCRIPTION
  Installs the treehouse binary (single binary from the GitHub release),
  verifies its SHA256 checksum, and optionally builds from source. This is
  the Windows companion to install.sh.

.EXAMPLE
  irm https://raw.githubusercontent.com/quangdang46/treehouse_rust/main/install.ps1 | iex

.PARAMETER Dest
  Install destination directory (default: $HOME\.local\bin).

.PARAMETER Version
  Pin a specific version tag (e.g. v0.1.0). Defaults to latest.

.PARAMETER EasyMode
  Prepend the destination to the user PATH automatically.

.PARAMETER Verify
  Run the binary's --version after install to confirm success.

.PARAMETER FromSource
  Build from source with cargo instead of downloading a release asset.

.PARAMETER Uninstall
  Remove the binary and PATH entries.
#>
param(
    [string]$Dest = "$env:USERPROFILE\.local\bin",
    [string]$Version = "",
    [string]$Repo = "treehouse_rust",
    [string]$Owner = "quangdang46",
    [string]$BinaryName = "treehouse",
    [switch]$EasyMode,
    [switch]$Verify,
    [switch]$FromSource,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

# === TLS 1.2 (required by GitHub; default on older PowerShell is TLS 1.0) ===
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 -bor [Net.SecurityProtocolType]::Tls13
} catch {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
}

function Log-Info($msg) { if (-not $script:Quiet) { Write-Host "[$BinaryName] $msg" -ForegroundColor Blue } }
function Log-Warn($msg) { Write-Host "[$BinaryName] WARN: $msg" -ForegroundColor Yellow }
function Log-Success($msg) { if (-not $script:Quiet) { Write-Host "[OK] $msg" -ForegroundColor Green } }
function Die($msg) { Write-Host "ERROR: $msg" -ForegroundColor Red; exit 1 }

$script:Quiet = $false
$script:MaxRetries = 3

# === Uninstall ===
if ($Uninstall) {
    Remove-Item -Force "$Dest\$BinaryName.exe" -ErrorAction SilentlyContinue
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath) {
        $newPath = ($userPath -split ';' | Where-Object { $_ -ne $Dest }) -join ';'
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    }
    Write-Host "[OK] $BinaryName uninstalled from $Dest"
    exit 0
}

# === Platform detection ===
$platform = switch ($true) {
    ([Environment]::Is64BitOperatingSystem -and $env:PROCESSOR_ARCHITECTURE -eq "AMD64") { "windows_x86_64" }
    ([Environment]::Is64BitOperatingSystem -and $env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "windows_aarch64" }
    default { Die "Unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}

# === Version resolution ===
function Resolve-Version {
    if ($Version) { return $Version }
    Log-Info "Resolving latest version..."
    $apiUrl = "https://api.github.com/repos/$Owner/$Repo/releases/latest"
    try {
        $release = Invoke-RestMethod -Uri $apiUrl -Headers @{"Accept"="application/vnd.github.v3+json"} -TimeoutSec 30
        return $release.tag_name
    } catch {
        # Fallback: resolve the redirect URL.
        $req = [System.Net.WebRequest]::Create("https://github.com/$Owner/$Repo/releases/latest")
        $req.AllowAutoRedirect = $false
        try {
            $resp = $req.GetResponse()
        } catch [System.Net.WebException] {
            $resp = $_.Exception.Response
        }
        $location = $resp.PSObject.Properties["ResponseUri"] | Select-Object -ExpandProperty Value
        if ($location -and "$location" -match "/tag/(v[0-9][^/]+)") {
            return $matches[1]
        }
        Die "Could not resolve the latest version. Check: https://github.com/$Owner/$Repo/releases"
    }
}

# === Download with retry ===
function Download-File {
    param([string]$Url, [string]$OutFile, [int]$MaxRetries = 3)
    $attempt = 0
    while ($attempt -lt $MaxRetries) {
        $attempt++
        try {
            # Use -UseBasicParsing for compatibility (no IE/Edge dependency).
            # GitHub release assets use 302 redirects -- Invoke-WebRequest follows them.
            Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing -TimeoutSec 120 -ErrorAction Stop
            if ((Test-Path $OutFile) -and (Get-Item $OutFile).Length -gt 0) {
                return $true
            }
            throw "Downloaded file is empty or missing"
        } catch {
            $errMsg = $_.Exception.Message
            if ($attempt -lt $MaxRetries) {
                Log-Warn "Download attempt $attempt/$MaxRetries failed: $errMsg -- retrying in 3s..."
                Start-Sleep -Seconds 3
            } else {
                Log-Warn "Download failed after $MaxRetries attempts: $errMsg"
                return $false
            }
        }
    }
    return $false
}

# === Main install ===
function Main {
    $resolvedVersion = Resolve-Version
    Log-Info "Platform: $platform | Version: $resolvedVersion | Destination: $Dest"

    if ($FromSource) {
        Log-Info "Building from source..."
        if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Die "cargo not found. Install Rust: https://rustup.rs" }
        $tmpSrc = Join-Path $env:TEMP "$BinaryName-src"
        if (Test-Path $tmpSrc) { Remove-Item -Recurse -Force $tmpSrc }
        git clone --depth 1 "https://github.com/$Owner/$Repo.git" $tmpSrc
        Push-Location $tmpSrc
        try {
            $env:CARGO_TARGET_DIR = Join-Path $env:TEMP "$BinaryName-target"
            cargo build --release --bin $BinaryName
        } finally {
            Pop-Location
        }
        $binPath = Join-Path $env:CARGO_TARGET_DIR "release\$BinaryName.exe"
        if (-not (Test-Path $binPath)) { Die "Build did not produce $binPath" }
    } else {
        $parts = $platform.Split("_", 2)
        $os = $parts[0]
        $arch = $parts[1]
        $archiveName = "$BinaryName-$resolvedVersion-$os-$arch.zip"
        $url = "https://github.com/$Owner/$Repo/releases/download/$resolvedVersion/$archiveName"
        $tmpDir = Join-Path $env:TEMP ([System.Guid]::NewGuid().ToString())
        New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null
        $zipPath = Join-Path $tmpDir $archiveName

        Log-Info "Downloading $url"
        if (-not (Download-File -Url $url -OutFile $zipPath)) {
            Log-Warn "Binary download failed -- building from source..."
            $FromSource = $true
            Main; return
        }

        # Checksum sidecar (optional) is named `<archive-base>.sha256` (no `.zip`).
        $checksumUrl = "$url.sha256" -replace '\.zip$', ''
        try {
            Invoke-WebRequest -Uri $checksumUrl -OutFile "$zipPath.sha256" -UseBasicParsing -TimeoutSec 30
            $expected = (Get-Content "$zipPath.sha256" -First 1).Split()[0]
            $actual = (Get-FileHash -Algorithm SHA256 -Path $zipPath).Hash.ToLower()
            if ($expected -ne $actual) { Die "Checksum mismatch! Expected: $expected, Got: $actual" }
            Log-Info "Checksum verified"
        } catch {
            Log-Warn "No checksum sidecar found -- skipping verification"
        }

        $zipExtract = Join-Path $tmpDir "extract"
        Expand-Archive -Path $zipPath -DestinationPath $zipExtract -Force

        # Walk the extracted tree for the executable by name.
        $binPath = Get-ChildItem -Path $zipExtract -Filter "$BinaryName.exe" -Recurse -File |
                   Where-Object { $_.Attributes -notmatch "Directory" } |
                   Select-Object -First 1 -ExpandProperty FullName
        if (-not $binPath) { Die "Binary '$BinaryName.exe' not found inside archive" }
    }

    # === Atomic install ===
    if (-not (Test-Path $Dest)) { New-Item -ItemType Directory -Path $Dest -Force | Out-Null }
    $finalPath = Join-Path $Dest "$BinaryName.exe"
    $tmpInstall = "$finalPath.tmp.$(Get-Random)"
    Move-Item -Force $binPath $tmpInstall
    Move-Item -Force $tmpInstall $finalPath

    # === PATH (easy-mode) ===
    if ($EasyMode) {
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        if ($userPath -notmatch [regex]::Escape($Dest)) {
            $newPath = "$Dest;$userPath"
            [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
            $env:Path = "$Dest;$env:Path"
            Log-Warn "PATH updated -- restart your shell to pick up the change"
        }
    } else {
        Log-Warn "Add to PATH: `$env:Path = `"$Dest;`$env:Path`""
    }

    if ($Verify) {
        $v = & $finalPath --version 2>$null
        if ($LASTEXITCODE -ne 0) { Die "Post-install verification failed" }
        $v
    }

    Write-Host ""
    Write-Host "[OK] $BinaryName installed -> $finalPath"
    $ver = & $finalPath --version 2>$null
    Write-Host "  $ver"
    Write-Host ""
    Write-Host "  Get started:  $BinaryName --help"
}

Main
