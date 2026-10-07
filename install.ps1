# Smartloop CLI installer for Windows.
#
#   irm https://smartloop.ai/install.ps1 | iex
#
# Environment:
#   SMARTLOOP_CLI_VERSION      Version to install (default: latest release)
#   SMARTLOOP_CLI_INSTALL_DIR  Install directory (default: $CARGO_HOME\bin when a
#                              Rust toolchain is present, else $HOME\.smartloop\bin)

$ErrorActionPreference = 'Stop'

$Repo = 'smartloop-ai/docent'

# Where the binary lands.  A Rust toolchain already has CARGO_HOME\bin on PATH,
# so installing there means `smartloop` works straight away with no PATH edit.
# Without one, fall back to our own directory -- $HOME\.smartloop is the
# studio's data directory, so the binary sits beside it, not inside the data.
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
$cargoBin = Join-Path $cargoHome 'bin'

$InstallDir = if ($env:SMARTLOOP_CLI_INSTALL_DIR) {
    $env:SMARTLOOP_CLI_INSTALL_DIR
} elseif (Test-Path -Path $cargoBin -PathType Container) {
    $cargoBin
} else {
    Join-Path $HOME '.smartloop\bin'
}

# ANSI colors. Windows Terminal and PowerShell 7 render these; older consoles
# on Windows 10+ do too once VT processing is on, which it is by default.
$esc = [char]27
$MUTED = "$esc[0;2m"; $PINK = "$esc[38;5;205m"; $GREEN = "$esc[1;32m"
$BOLD = "$esc[1m"; $NC = "$esc[0m"

function Write-Banner {
    param([string]$Version, [string]$Dir)

    # Built from char codes rather than `u{2588} escapes: `u{} is PowerShell 7
    # only, and `irm | iex` runs under Windows PowerShell 5.1 on a stock box.
    # Literal block characters in the source would be just as risky, since 5.1
    # decodes a BOM-less script as ANSI.
    $full = [char]0x2588   # full block
    $upper = [char]0x2580  # upper half block
    $lower = [char]0x2584  # lower half block
    $render = {
        param($template)
        $template.Replace('#', $full).Replace('^', $upper).Replace('<', $lower)
    }

    Write-Host ""
    Write-Host "$PINK$(& $render '#^^^< <^^^< <^^^^ #^^^^ #<  # ^^#^^')$NC"
    Write-Host "$PINK$(& $render '#   # #   # #     #^^^  # ^<#   #')$NC  ${MUTED}by Smartloop${NC}"
    Write-Host "$PINK$(& $render '^^^^   ^^^   ^^^^ ^^^^^ ^   ^   ^')$NC"
    Write-Host "${MUTED}Your private AI assistant${NC}"
    Write-Host ""
    Write-Host "${MUTED}Version: ${NC}$Version"
    Write-Host ""

    if (($env:Path -split ';') -contains $Dir) {
        Write-Host "${MUTED}To get started:${NC}"
    } else {
        Write-Host "${MUTED}To get started, restart your terminal, then run:${NC}"
    }
    Write-Host ""
    Write-Host "  docent                  ${MUTED}# Start chatting${NC}"
    Write-Host ""
    Write-Host "${MUTED}For more information visit ${NC}https://smartloop.ai/docs/intro/"
    Write-Host ""
}

function Get-LatestVersion {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
        -Headers @{ 'User-Agent' = 'smartloop-cli-installer' }
    return $release.tag_name -replace '^v', ''
}

# Only x86_64 Windows binaries are published; ARM64 Windows runs them under
# emulation, so no separate target is needed.
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne 'AMD64' -and $arch -ne 'ARM64') {
    throw "Unsupported architecture: $arch"
}
$target = 'x86_64-pc-windows-msvc'

$version = if ($env:SMARTLOOP_CLI_VERSION) {
    $env:SMARTLOOP_CLI_VERSION -replace '^v', ''
} else {
    Get-LatestVersion
}
if (-not $version) { throw 'Could not determine the latest release; set SMARTLOOP_CLI_VERSION' }

$name = "smartloop-$version-$target"
$archive = "$name.zip"
$baseUrl = "https://github.com/$Repo/releases/download/v$version"

Write-Host "${MUTED}Reading package lists... Done$NC"
Write-Host "${MUTED}The following NEW packages will be installed:$NC"
Write-Host "  ${BOLD}smartloop${NC} ${MUTED}($version, $target)${NC}"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    Write-Host "${MUTED}[1/3] Downloading docent ($version)${NC}"
    $archivePath = Join-Path $tmp $archive
    # Invoke-WebRequest draws its own progress bar; leave $ProgressPreference
    # at its default so the user sees it.
    Invoke-WebRequest -Uri "$baseUrl/$archive" -OutFile $archivePath

    $sumsPath = Join-Path $tmp 'SHA256SUMS'
    Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile $sumsPath

    $expected = (Get-Content $sumsPath |
        Where-Object { $_ -match "\s$([regex]::Escape($archive))$" } |
        ForEach-Object { ($_ -split '\s+')[0] })
    if (-not $expected) { throw "No checksum published for $archive" }

    $actual = (Get-FileHash -Path $archivePath -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected.ToLower()) { throw "Checksum mismatch for $archive" }

    Write-Host "${MUTED}[2/3] Unpacking docent ($version)${NC}"
    Expand-Archive -Path $archivePath -DestinationPath $tmp -Force

    Write-Host "${MUTED}[3/3] Setting up docent ($version)${NC}"
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Path (Join-Path $tmp "$name\smartloop.exe") -Destination $InstallDir -Force
    # The Vulkan loader and MSVC runtime the embedded agent loads, found in
    # the application directory.
    Copy-Item -Path (Join-Path $tmp "$name\*.dll") -Destination $InstallDir -Force

    # `docent` is the same binary under its other name; symlinks need admin.
    Copy-Item -Path (Join-Path $InstallDir 'smartloop.exe') -Destination (Join-Path $InstallDir 'docent.exe') -Force

    Write-Host "${GREEN}Installed${NC} $(Join-Path $InstallDir 'smartloop.exe') ${MUTED}(also as${NC} docent${MUTED})${NC}"

    Write-Host "${MUTED}Processing triggers for docent ($version) ...$NC"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable('Path', "$InstallDir;$userPath", 'User')
        Write-Host "${MUTED}Added ${NC}smartloop${MUTED} to your PATH (${InstallDir})${NC}"
    }

    Write-Banner -Version $version -Dir $InstallDir
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
