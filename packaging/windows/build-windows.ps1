# Builds Flux for Windows: release executables, portable ZIP and (when Inno
# Setup is available) a per-user installer. It never publishes anything.
#
# Usage (from PowerShell):
#   packaging/windows/build-windows.ps1 -Architecture x86_64
#   packaging/windows/build-windows.ps1 -Architecture arm64 -SkipInstaller
#
# The executable icon/version resource is embedded by crates/flux-gui/build.rs
# through the Visual Studio resource compiler. Unsigned installers trigger
# normal SmartScreen warnings; signing is a separate authorized release step.

[CmdletBinding()]
param(
    [ValidateSet('x86_64', 'arm64')]
    [string]$Architecture = 'x86_64',
    [switch]$SkipBuild,
    [switch]$SkipInstaller
)

$ErrorActionPreference = 'Stop'
$Root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
Set-Location $Root

$Target = if ($Architecture -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$Meta = (cargo metadata --no-deps --format-version 1 --locked | ConvertFrom-Json)
$Version = ($Meta.packages | Where-Object { $_.name -eq 'flux-gui' } | Select-Object -First 1).version
if (-not $Version) { throw 'Could not determine the workspace version.' }

$TargetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
$BinaryDir = Join-Path $TargetRoot (Join-Path $Target 'release')
$Dist = if ($env:FLUX_DIST_DIR) { $env:FLUX_DIST_DIR } else { Join-Path $Root 'dist' }
$Name = "Flux-$Version-windows-$Architecture"
$Stage = Join-Path $Dist $Name
New-Item -ItemType Directory -Force -Path $Dist | Out-Null

if (-not $SkipBuild) {
    cargo build --release --locked --target $Target -p flux-gui -p flux-tui --features cpal
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
}

$Gui = Join-Path $BinaryDir 'flux.exe'
$Tui = Join-Path $BinaryDir 'flux-tui.exe'
foreach ($exe in @($Gui, $Tui)) {
    if (-not (Test-Path $exe)) { throw "Missing release executable: $exe" }
}

if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
Copy-Item $Gui $Stage
Copy-Item $Tui $Stage
foreach ($file in @('README.md', 'LICENSE', 'THIRD_PARTY_NOTICES.md')) {
    if (Test-Path (Join-Path $Root $file)) { Copy-Item (Join-Path $Root $file) $Stage }
}
$LicenseDir = Join-Path $Stage 'licenses'
New-Item -ItemType Directory -Force -Path $LicenseDir | Out-Null
Copy-Item (Join-Path $Root 'packaging\licenses\*.txt') $LicenseDir
@'
Flux for Windows
================

Flux is a local-first desktop music player. It stores its canonical library and
settings under your per-user application data, never in this folder.

Portable use
------------
Run `flux.exe` from this folder. `flux-tui.exe` is the terminal interface.
The portable folder can be moved or deleted; user data remains in
`%LOCALAPPDATA%\flux` and `%APPDATA%\flux`.

Installer
---------
The optional `*-setup.exe` installs per-user by default and creates a Start
Menu shortcut. Uninstalling removes application files only; it does not delete
your music, library, settings, ratings, history, lyrics or artwork.

Unsigned builds
---------------
These artifacts are not code-signed. Windows SmartScreen may warn on first
launch. Verify the SHA-256 checksum published with the artifact. Signing and
publisher identity are only added in an authorized signing release.

Requirements
------------
Windows 10 or later (x64 or ARM64 matching the artifact). No account or
network service is required.
'@ | Set-Content -Encoding UTF8 (Join-Path $Stage 'README-windows.txt')

Add-Type -AssemblyName System.Drawing
$Icon = [System.Drawing.Icon]::ExtractAssociatedIcon($Gui)
if (-not $Icon) { throw 'flux.exe has no embedded icon resource.' }
$Icon.Dispose()

$Zip = Join-Path $Dist "$Name.zip"
if (Test-Path $Zip) { Remove-Item -Force $Zip }
Compress-Archive -Path (Join-Path $Stage '*') -DestinationPath $Zip -CompressionLevel Optimal

$Installer = $null
if (-not $SkipInstaller) {
    $Candidates = @(
        (Get-Command ISCC.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -ErrorAction SilentlyContinue),
        (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'),
        (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe')
    ) | Where-Object { $_ -and (Test-Path $_) }
    $Iscc = $Candidates | Select-Object -First 1
    if (-not $Iscc) {
        Write-Warning 'Inno Setup 6 (ISCC.exe) was not found; portable ZIP only. Install Inno Setup for the installer artifact.'
    } else {
        & $Iscc `
            "/DMyAppVersion=$Version" `
            "/DMyArch=$Architecture" `
            "/DMyStage=$Stage" `
            "/DMyOutput=$Dist" `
            (Join-Path $Root 'packaging\windows\flux.iss')
        if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation failed.' }
        $Installer = Join-Path $Dist "Flux-$Version-windows-$Architecture-setup.exe"
        if (-not (Test-Path $Installer)) { throw 'Installer artifact was not produced.' }
    }
}

$Artifacts = @($Zip) + @($Installer) | Where-Object { $_ }
$Checksums = foreach ($artifact in $Artifacts) {
    $hash = (Get-FileHash -Algorithm SHA256 -Path $artifact).Hash.ToLowerInvariant()
    "$hash  $(Split-Path $artifact -Leaf)"
}
$Checksums | Set-Content -Encoding ASCII (Join-Path $Dist 'SHA256SUMS-windows.txt')

Write-Host "Windows artifacts in $Dist"
$Artifacts | ForEach-Object { Write-Host "  $_ ($([math]::Round((Get-Item $_).Length / 1MB, 1)) MiB)" }
Write-Host 'Unsigned artifacts: SmartScreen warnings are expected until an authorized signing step.'
