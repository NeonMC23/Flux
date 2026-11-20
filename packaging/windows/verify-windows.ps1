# Verifies Windows release artifacts on a disposable Windows runner or machine.
# Does not sign or publish anything. The optional installer lifecycle test writes
# only to the CI temp directory and the per-user Flux data directory.

[CmdletBinding()]
param(
    [string]$Architecture = 'x86_64',
    [switch]$TestInstaller
)

$ErrorActionPreference = 'Stop'
$Root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$Meta = (cargo metadata --no-deps --format-version 1 --locked | ConvertFrom-Json)
$Version = ($Meta.packages | Where-Object { $_.name -eq 'flux-gui' } | Select-Object -First 1).version
$Dist = if ($env:FLUX_DIST_DIR) { $env:FLUX_DIST_DIR } else { Join-Path $Root 'dist' }
$Name = "Flux-$Version-windows-$Architecture"
$Zip = Join-Path $Dist "$Name.zip"
$Installer = Join-Path $Dist "$Name-setup.exe"
if (-not (Test-Path $Zip)) { throw "Missing portable ZIP: $Zip" }

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("flux-verify-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null
try {
    Expand-Archive -Path $Zip -DestinationPath $Tmp
    $Gui = Join-Path $Tmp 'flux.exe'
    $Tui = Join-Path $Tmp 'flux-tui.exe'
    foreach ($exe in @($Gui, $Tui)) {
        if (-not (Test-Path $exe)) { throw "Missing executable in ZIP: $exe" }
    }
    $Expected = "Flux $Version (io.github.neonmc23.flux)"
    function Get-CliOutput([string]$Executable, [string]$Argument) {
        $stdout = Join-Path $Tmp ("stdout-" + [guid]::NewGuid() + ".txt")
        $stderr = Join-Path $Tmp ("stderr-" + [guid]::NewGuid() + ".txt")
        $process = Start-Process -FilePath $Executable -ArgumentList $Argument -Wait -PassThru `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        if ($process.ExitCode -ne 0) {
            throw "$Executable $Argument exited $($process.ExitCode): $(Get-Content $stderr -Raw)"
        }
        return (Get-Content $stdout -Raw).Trim()
    }
    if ((Get-CliOutput $Gui '--version') -ne $Expected) { throw 'GUI --version mismatch.' }
    if ((Get-CliOutput $Tui '--version') -ne $Expected) { throw 'TUI --version mismatch.' }
    if ((Get-CliOutput $Gui '--help') -notmatch 'Usage: flux') { throw 'GUI --help mismatch.' }

    Add-Type -AssemblyName System.Drawing
    $Icon = [System.Drawing.Icon]::ExtractAssociatedIcon($Gui)
    if (-not $Icon) { throw 'flux.exe has no embedded icon.' }
    $Icon.Dispose()

    foreach ($forbidden in @('FluxLibrary', 'queue.json', 'ratings.flux', 'history.flux')) {
        if (Get-ChildItem -Recurse -File $Tmp | Where-Object { $_.Name -eq $forbidden }) {
            throw "Portable ZIP contains user data: $forbidden"
        }
    }

    $Checksums = Join-Path $Dist 'SHA256SUMS-windows.txt'
    if (-not (Test-Path $Checksums)) { throw 'Missing SHA256SUMS-windows.txt' }
    $expectedHash = (Get-FileHash -Algorithm SHA256 $Zip).Hash.ToLowerInvariant()
    $recorded = Get-Content $Checksums | Where-Object { $_ -match [regex]::Escape((Split-Path $Zip -Leaf)) }
    if (-not $recorded -or -not $recorded.ToLowerInvariant().StartsWith($expectedHash)) {
        throw 'Portable ZIP checksum does not match SHA256SUMS-windows.txt.'
    }

    if ($TestInstaller) {
        if (-not (Test-Path $Installer)) { throw "Missing installer: $Installer" }
        $InstallDir = Join-Path $Tmp 'installed'
        $DataRoot = Join-Path $env:LOCALAPPDATA 'flux'
        $ConfigRoot = Join-Path $env:APPDATA 'flux'
        New-Item -ItemType Directory -Force -Path $DataRoot, $ConfigRoot | Out-Null
        Set-Content -Encoding UTF8 (Join-Path $DataRoot 'verify-sentinel') 'keep'
        Set-Content -Encoding UTF8 (Join-Path $ConfigRoot 'verify-sentinel') 'keep'
        $process = Start-Process -FilePath $Installer -Wait -PassThru -ArgumentList @(
            '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-',
            "/DIR=`"$InstallDir`""
        )
        if ($process.ExitCode -ne 0) { throw "Installer exited $($process.ExitCode)." }
        if ((Get-CliOutput (Join-Path $InstallDir 'flux.exe') '--version') -ne $Expected) {
            throw 'Installed GUI --version mismatch.'
        }
        $uninstaller = Join-Path $InstallDir 'unins000.exe'
        if (-not (Test-Path $uninstaller)) { throw 'Installer did not create unins000.exe.' }
        $uninstall = Start-Process -FilePath $uninstaller -Wait -PassThru -ArgumentList @(
            '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
        )
        if ($uninstall.ExitCode -ne 0) { throw "Uninstaller exited $($uninstall.ExitCode)." }
        if ((Get-Content (Join-Path $DataRoot 'verify-sentinel')).Trim() -ne 'keep') {
            throw 'Uninstall removed user data.'
        }
        if ((Get-Content (Join-Path $ConfigRoot 'verify-sentinel')).Trim() -ne 'keep') {
            throw 'Uninstall removed user config.'
        }
    }

    Write-Host "Windows artifact verification passed for $Name"
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
