<#
.SYNOPSIS
    Fail-closed SHA-256 verified Windows installer for AutoAgentFactory (`aaf.exe` and `aaf-hook.exe`).

.DESCRIPTION
    Downloads the official `x86_64-pc-windows-msvc` release archive and `SHA256SUMS.txt`
    from GitHub Releases, verifies the archive byte-for-byte with SHA-256 before extraction,
    installs `aaf.exe` and `aaf-hook.exe`, and adds the install directory to the user PATH.

.EXAMPLE
    irm https://raw.githubusercontent.com/ToanMobile/AutoAgentFactory/main/scripts/install.ps1 | iex

.EXAMPLE
    .\scripts\install.ps1 -Version v0.1.0 -InstallDir "$env:LOCALAPPDATA\Programs\aaf\bin"
#>

[CmdletBinding()]
param(
    [string]$Version = $env:AAF_VERSION,
    [string]$InstallDir = $(
        if ($env:AAF_INSTALL_DIR) {
            $env:AAF_INSTALL_DIR
        } else {
            Join-Path $env:LOCALAPPDATA "Programs\aaf\bin"
        }
    ),
    [string]$Repo = $(
        if ($env:AAF_REPO) {
            $env:AAF_REPO
        } else {
            "ToanMobile/AutoAgentFactory"
        }
    ),
    [switch]$FromSource
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# Enforce TLS 1.2+ on Windows PowerShell 5.1.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

function Fail([string]$Message) {
    Write-Error "aaf install failed: $Message"
    exit 1
}

function Assert-ValidRepo([string]$Slug) {
    if ($Slug -notmatch '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$' -or $Slug.Contains('..')) {
        Fail "invalid repository slug '$Slug' (expected owner/repo)"
    }
}

function Assert-ValidVersion([string]$Tag) {
    if ($Tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') {
        Fail "invalid release tag '$Tag' (expected v<major>.<minor>.<patch>, e.g. v0.1.0)"
    }
}

function Resolve-LatestVersion([string]$Slug) {
    $apiUrl = "https://api.github.com/repos/$Slug/releases/latest"
    $headers = @{
        "User-Agent" = "aaf-installer"
        "Accept"     = "application/vnd.github+json"
    }
    $release = Invoke-RestMethod -Uri $apiUrl -Headers $headers -UseBasicParsing
    if (-not $release -or -not $release.tag_name) {
        Fail "could not determine latest release tag from $apiUrl"
    }
    return [string]$release.tag_name
}

function Parse-ExpectedSha256([string]$SumsPath, [string]$ArchiveName) {
    $expected = $null
    foreach ($rawLine in (Get-Content -Path $SumsPath -Encoding UTF8)) {
        $line = $rawLine.Trim()
        if ([string]::IsNullOrEmpty($line) -or $line.StartsWith('#')) {
            continue
        }
        if ($line -match '^([0-9A-Fa-f]{64})\s+\*?(.+)$') {
            $hash = $Matches[1].ToLowerInvariant()
            $name = $Matches[2].Trim()
            if ($name -eq $ArchiveName) {
                if ($null -ne $expected -and $expected -ne $hash) {
                    Fail "conflicting duplicate SHA-256 entries for $ArchiveName in SHA256SUMS.txt"
                }
                $expected = $hash
            }
        }
    }
    if (-not $expected) {
        Fail "SHA-256 entry for '$ArchiveName' not found in SHA256SUMS.txt"
    }
    return $expected
}

function Verify-ArchiveSha256([string]$ArchivePath, [string]$ArchiveName, [string]$SumsPath) {
    $expected = Parse-ExpectedSha256 -SumsPath $SumsPath -ArchiveName $ArchiveName
    $actual = (Get-FileHash -Path $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Write-Host "SHA-256 verification FAILED for $ArchiveName" -ForegroundColor Red
        Write-Host "  expected: $expected" -ForegroundColor Red
        Write-Host "  actual:   $actual" -ForegroundColor Red
        Fail "checksum mismatch (aborting before extraction)"
    }
    Write-Host "Verified SHA-256 for $ArchiveName ($actual)"
}

function Install-BinaryAtomic([string]$SourcePath, [string]$DestDir, [string]$BinaryName) {
    if (-not (Test-Path -LiteralPath $SourcePath -PathType Leaf)) {
        Fail "binary missing: $BinaryName ($SourcePath)"
    }
    New-Item -ItemType Directory -Path $DestDir -Force | Out-Null
    $destPath = Join-Path $DestDir $BinaryName
    $stagedPath = Join-Path $DestDir (".$BinaryName.new.$PID")
    $backupPath = Join-Path $DestDir (".$BinaryName.bak.$PID")

    Copy-Item -LiteralPath $SourcePath -Destination $stagedPath -Force
    try {
        if (Test-Path -LiteralPath $destPath) {
            Move-Item -LiteralPath $destPath -Destination $backupPath -Force
        }
        Move-Item -LiteralPath $stagedPath -Destination $destPath -Force
        if (Test-Path -LiteralPath $backupPath) {
            Remove-Item -LiteralPath $backupPath -Force -ErrorAction SilentlyContinue
        }
    } catch {
        if (Test-Path -LiteralPath $backupPath -and -not (Test-Path -LiteralPath $destPath)) {
            Move-Item -LiteralPath $backupPath -Destination $destPath -Force -ErrorAction SilentlyContinue
        }
        Remove-Item -LiteralPath $stagedPath -Force -ErrorAction SilentlyContinue
        throw
    }
}

function Ensure-UserPath([string]$Dir) {
    $normalized = $Dir.TrimEnd('\')
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @()
    if (-not [string]::IsNullOrWhiteSpace($userPath)) {
        $entries = $userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    }
    $alreadyPresent = $false
    foreach ($entry in $entries) {
        if ($entry.TrimEnd('\').Equals($normalized, [System.StringComparison]::OrdinalIgnoreCase)) {
            $alreadyPresent = $true
            break
        }
    }
    if (-not $alreadyPresent) {
        $newPath = if ($entries.Count -gt 0) {
            ($entries + $normalized) -join ';'
        } else {
            $normalized
        }
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        Write-Host "Added $normalized to user PATH (open a new terminal to pick it up)."
    }

    $procEntries = @($env:PATH -split ';')
    $inProc = $false
    foreach ($entry in $procEntries) {
        if ($entry.TrimEnd('\').Equals($normalized, [System.StringComparison]::OrdinalIgnoreCase)) {
            $inProc = $true
            break
        }
    }
    if (-not $inProc) {
        $env:PATH = "$normalized;$env:PATH"
    }
}

function Install-FromSource([string]$Slug, [string]$Tag, [string]$DestDir, [string]$WorkDir) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Fail "cargo is required for -FromSource"
    }
    if ((Test-Path "Cargo.toml") -and (Test-Path "crates\aaf-cli") -and (Test-Path "crates\aaf-hook")) {
        Write-Host "Building aaf.exe and aaf-hook.exe from local checkout..."
        & cargo build --release --locked -p aaf-cli -p aaf-hook
        if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }
        Install-BinaryAtomic -SourcePath "target\release\aaf.exe" -DestDir $DestDir -BinaryName "aaf.exe"
        Install-BinaryAtomic -SourcePath "target\release\aaf-hook.exe" -DestDir $DestDir -BinaryName "aaf-hook.exe"
    } else {
        if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
            Fail "git is required for -FromSource outside a local checkout"
        }
        $srcDir = Join-Path $WorkDir "src"
        $repoUrl = "https://github.com/$Slug.git"
        if (-not [string]::IsNullOrWhiteSpace($Tag)) {
            Assert-ValidVersion $Tag
            & git clone --depth 1 --branch $Tag $repoUrl $srcDir
        } else {
            & git clone --depth 1 $repoUrl $srcDir
        }
        if ($LASTEXITCODE -ne 0) { Fail "git clone failed" }
        Push-Location $srcDir
        try {
            & cargo build --release --locked -p aaf-cli -p aaf-hook
            if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }
        } finally {
            Pop-Location
        }
        Install-BinaryAtomic -SourcePath (Join-Path $srcDir "target\release\aaf.exe") -DestDir $DestDir -BinaryName "aaf.exe"
        Install-BinaryAtomic -SourcePath (Join-Path $srcDir "target\release\aaf-hook.exe") -DestDir $DestDir -BinaryName "aaf-hook.exe"
    }
}

Assert-ValidRepo $Repo

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("aaf-install-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null

try {
    if ($FromSource) {
        Install-FromSource -Slug $Repo -Tag $Version -DestDir $InstallDir -WorkDir $tempDir
    } else {
        if ([Environment]::Is64BitOperatingSystem -eq $false) {
            Fail "only 64-bit Windows (x86_64-pc-windows-msvc) is supported"
        }
        $target = "x86_64-pc-windows-msvc"
        if ([string]::IsNullOrWhiteSpace($Version)) {
            $Version = Resolve-LatestVersion -Slug $Repo
        }
        Assert-ValidVersion $Version

        $archiveName = "aaf-$Version-$target.zip"
        $baseUrl = "https://github.com/$Repo/releases/download/$Version"
        $archivePath = Join-Path $tempDir $archiveName
        $sumsPath = Join-Path $tempDir "SHA256SUMS.txt"

        Write-Host "Downloading $archiveName ($Version)..."
        Invoke-WebRequest -Uri "$baseUrl/$archiveName" -OutFile $archivePath -UseBasicParsing
        Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS.txt" -OutFile $sumsPath -UseBasicParsing

        Verify-ArchiveSha256 -ArchivePath $archivePath -ArchiveName $archiveName -SumsPath $sumsPath

        $extractDir = Join-Path $tempDir "extract"
        Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir -Force

        $nestedRoot = Join-Path $extractDir "aaf-$Version-$target"
        if (Test-Path -LiteralPath (Join-Path $nestedRoot "aaf.exe") -PathType Leaf) {
            $binRoot = $nestedRoot
        } elseif (Test-Path -LiteralPath (Join-Path $extractDir "aaf.exe") -PathType Leaf) {
            $binRoot = $extractDir
        } else {
            Fail "could not locate aaf.exe inside $archiveName"
        }

        Install-BinaryAtomic -SourcePath (Join-Path $binRoot "aaf.exe") -DestDir $InstallDir -BinaryName "aaf.exe"
        Install-BinaryAtomic -SourcePath (Join-Path $binRoot "aaf-hook.exe") -DestDir $InstallDir -BinaryName "aaf-hook.exe"
    }

    Ensure-UserPath -Dir $InstallDir

    Write-Host "Installed aaf.exe and aaf-hook.exe into $InstallDir"
    $aafExe = Join-Path $InstallDir "aaf.exe"
    & $aafExe --version
    if ($LASTEXITCODE -ne 0) {
        Fail "verification failed: $aafExe --version exited with $LASTEXITCODE"
    }
} finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}
