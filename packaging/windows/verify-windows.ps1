# Verifies Windows release artifacts on a disposable Windows runner or machine.
# Does not sign or publish anything. The optional installer lifecycle test writes
# only to the CI temp directory and redirected per-user Flux data directories.

[CmdletBinding()]
param(
    [string]$Architecture = 'x86_64',
    [switch]$TestInstaller,
    [switch]$TestGuiLaunch
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
    if ((Get-CliOutput $Tui '--help') -notmatch 'Usage: flux-tui') { throw 'TUI --help mismatch.' }

    # A release GUI is a Windows-subsystem executable (2); the TUI keeps the
    # console subsystem (3). Verify the PE header rather than inferring it
    # from packaging behavior.
    function Get-PeSubsystem([string]$Path) {
        $bytes = [System.IO.File]::ReadAllBytes($Path)
        if ($bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) {
            throw "$Path is not a PE executable (missing MZ header)."
        }
        $pe = [BitConverter]::ToInt32($bytes, 0x3C)
        if ($bytes[$pe] -ne 0x50 -or $bytes[$pe + 1] -ne 0x45) {
            throw "$Path is not a PE executable (missing PE signature)."
        }
        $optional = $pe + 24
        return [BitConverter]::ToUInt16($bytes, $optional + 0x5C)
    }
    if ((Get-PeSubsystem $Gui) -ne 2) { throw 'flux.exe is not a Windows GUI-subsystem executable.' }
    if ((Get-PeSubsystem $Tui) -ne 3) { throw 'flux-tui.exe is not a console-subsystem executable.' }

    $Info = (Get-Item $Gui).VersionInfo
    if ($Info.ProductName -ne 'Flux') { throw "flux.exe ProductName is '$($Info.ProductName)'." }
    if ($Info.CompanyName -ne 'NeonMC23') { throw "flux.exe CompanyName is '$($Info.CompanyName)'." }
    if (-not $Info.ProductVersion.StartsWith($Version)) {
        throw "flux.exe ProductVersion is '$($Info.ProductVersion)', expected '$Version'."
    }
    if ($Info.FileDescription -notmatch 'Flux') { throw 'flux.exe has no Flux file description.' }

    Add-Type -AssemblyName System.Drawing
    $Icon = [System.Drawing.Icon]::ExtractAssociatedIcon($Gui)
    if (-not $Icon) { throw 'flux.exe has no embedded icon.' }
    $Icon.Dispose()

    foreach ($forbidden in @('FluxLibrary', 'queue.json', 'ratings.flux', 'history.flux')) {
        if (Get-ChildItem -Recurse -File $Tmp | Where-Object { $_.Name -eq $forbidden }) {
            throw "Portable ZIP contains user data: $forbidden"
        }
    }

    # Exercise a non-ASCII extraction directory and executable path.
    $UnicodeDir = Join-Path $Tmp 'ünicode 音乐'
    New-Item -ItemType Directory -Force -Path $UnicodeDir | Out-Null
    Expand-Archive -Path $Zip -DestinationPath $UnicodeDir
    if ((Get-CliOutput (Join-Path $UnicodeDir 'flux.exe') '--version') -ne $Expected) {
        throw 'GUI --version failed from a Unicode path.'
    }
    if ((Get-CliOutput (Join-Path $UnicodeDir 'flux-tui.exe') '--version') -ne $Expected) {
        throw 'TUI --version failed from a Unicode path.'
    }

    $Checksums = Join-Path $Dist 'SHA256SUMS-windows.txt'
    if (-not (Test-Path $Checksums)) { throw 'Missing SHA256SUMS-windows.txt' }
    $expectedHash = (Get-FileHash -Algorithm SHA256 $Zip).Hash.ToLowerInvariant()
    $recorded = Get-Content $Checksums | Where-Object { $_ -match [regex]::Escape((Split-Path $Zip -Leaf)) }
    if (-not $recorded -or -not $recorded.ToLowerInvariant().StartsWith($expectedHash)) {
        throw 'Portable ZIP checksum does not match SHA256SUMS-windows.txt.'
    }

    if ($TestGuiLaunch) {
        $oldLocal = $env:LOCALAPPDATA
        $oldRoaming = $env:APPDATA
        try {
            $env:LOCALAPPDATA = Join-Path $Tmp 'appdata-local'
            $env:APPDATA = Join-Path $Tmp 'appdata-roaming'
            New-Item -ItemType Directory -Force -Path $env:LOCALAPPDATA, $env:APPDATA | Out-Null
            $Library = Join-Path $env:LOCALAPPDATA 'flux\FluxLibrary'
            $Catalog = Join-Path $Library 'catalog.flux'

            $first = Start-Process -FilePath $Gui -PassThru
            $deadline = (Get-Date).AddSeconds(20)
            while (-not (Test-Path $Catalog) -and (Get-Date) -lt $deadline) {
                if ($first.HasExited) {
                    throw "GUI exited $($first.ExitCode) before creating its first-run library."
                }
                Start-Sleep -Milliseconds 250
            }
            if (-not (Test-Path $Catalog)) { throw 'GUI did not create a first-run canonical library.' }
            Start-Sleep -Seconds 2
            if ($first.HasExited) { throw "GUI exited $($first.ExitCode) during first-run startup." }
            Stop-Process -Id $first.Id -Force
            $first.WaitForExit()

            $second = Start-Process -FilePath $Gui -PassThru
            Start-Sleep -Seconds 3
            if ($second.HasExited) {
                throw "GUI exited $($second.ExitCode) while reopening an existing library."
            }
            Stop-Process -Id $second.Id -Force
            $second.WaitForExit()
        } finally {
            $env:LOCALAPPDATA = $oldLocal
            $env:APPDATA = $oldRoaming
        }
    }

    if ($TestInstaller) {
        if (-not (Test-Path $Installer)) { throw "Missing installer: $Installer" }
        $oldLocal = $env:LOCALAPPDATA
        $oldRoaming = $env:APPDATA
        try {
            $env:LOCALAPPDATA = Join-Path $Tmp 'installer-local'
            $env:APPDATA = Join-Path $Tmp 'installer-roaming'
            $InstallDir = Join-Path $Tmp 'installed'
            $DataRoot = Join-Path $env:LOCALAPPDATA 'flux'
            $ConfigRoot = Join-Path $env:APPDATA 'flux'
            New-Item -ItemType Directory -Force -Path $DataRoot, $ConfigRoot | Out-Null
            Set-Content -Encoding UTF8 (Join-Path $DataRoot 'verify-sentinel') 'keep'
            Set-Content -Encoding UTF8 (Join-Path $ConfigRoot 'verify-sentinel') 'keep'

            $arguments = @(
                '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-',
                "/DIR=`"$InstallDir`""
            )
            $process = Start-Process -FilePath $Installer -Wait -PassThru -ArgumentList $arguments
            if ($process.ExitCode -ne 0) { throw "Installer exited $($process.ExitCode)." }
            if ((Get-CliOutput (Join-Path $InstallDir 'flux.exe') '--version') -ne $Expected) {
                throw 'Installed GUI --version mismatch.'
            }
            $uninstaller = Join-Path $InstallDir 'unins000.exe'
            if (-not (Test-Path $uninstaller)) { throw 'Installer did not create unins000.exe.' }

            # Reinstall over the existing installation: application files must
            # be replaced and user data must remain untouched.
            $reinstall = Start-Process -FilePath $Installer -Wait -PassThru -ArgumentList $arguments
            if ($reinstall.ExitCode -ne 0) { throw "Reinstall exited $($reinstall.ExitCode)." }
            if ((Get-CliOutput (Join-Path $InstallDir 'flux.exe') '--version') -ne $Expected) {
                throw 'Reinstalled GUI --version mismatch.'
            }
            if ((Get-Content (Join-Path $DataRoot 'verify-sentinel')).Trim() -ne 'keep') {
                throw 'Reinstall removed user data.'
            }

            $uninstall = Start-Process -FilePath $uninstaller -Wait -PassThru -ArgumentList @(
                '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
            )
            if ($uninstall.ExitCode -ne 0) { throw "Uninstaller exited $($uninstall.ExitCode)." }
            if (Test-Path (Join-Path $InstallDir 'flux.exe')) {
                throw 'Uninstall left an application-owned executable behind.'
            }
            if ((Get-Content (Join-Path $DataRoot 'verify-sentinel')).Trim() -ne 'keep') {
                throw 'Uninstall removed user data.'
            }
            if ((Get-Content (Join-Path $ConfigRoot 'verify-sentinel')).Trim() -ne 'keep') {
                throw 'Uninstall removed user config.'
            }
        } finally {
            $env:LOCALAPPDATA = $oldLocal
            $env:APPDATA = $oldRoaming
        }
    }

    Write-Host "Windows artifact verification passed for $Name"
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
