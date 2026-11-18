[CmdletBinding()]
param(
    [switch]$FetchDrivers,
    [switch]$RequireDrivers,
    # Retained for compatibility; missing driver packages are allowed by default.
    [switch]$AllowMissingDrivers,
    [string]$ExpectedTag = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$script:NativeExitCode = 0
$FailureCode = 0
$Staging = $null

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)
    & $Command @Arguments
    $Code = $LASTEXITCODE
    if ($Code -ne 0) {
        $script:NativeExitCode = $Code
        throw "$Command failed with exit code $Code."
    }
}

Push-Location (Join-Path $PSScriptRoot '..')
try {
    if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitProcess) {
        throw 'Run this script in 64-bit PowerShell on Windows.'
    }
    if ($RequireDrivers -and $AllowMissingDrivers) {
        throw '-RequireDrivers and -AllowMissingDrivers cannot be combined.'
    }
    $MetadataText = & cargo metadata --locked --format-version 1 --filter-platform x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) {
        $script:NativeExitCode = $LASTEXITCODE
        throw 'cargo metadata failed.'
    }
    $Metadata = $MetadataText | ConvertFrom-Json
    $ManifestPath = [IO.Path]::GetFullPath((Join-Path (Get-Location) 'Cargo.toml'))
    $Application = @($Metadata.packages | Where-Object {
        [IO.Path]::GetFullPath($_.manifest_path) -eq $ManifestPath
    })
    if ($Application.Count -ne 1) { throw 'Could not locate the application in Cargo metadata.' }
    $Version = $Application[0].version
    if ($Version -notmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') {
        throw "Invalid release version in Cargo.toml: $Version"
    }
    if ($ExpectedTag -and $ExpectedTag -cne "v$Version") {
        throw "Tag $ExpectedTag does not match Cargo.toml version v$Version."
    }
    if (-not (Select-String -LiteralPath CHANGELOG.md -Pattern ('^## \[' + [regex]::Escape($Version) + '\](\s|$)') -Quiet)) {
        throw "CHANGELOG.md is missing release notes for $Version."
    }

    $DriversRequired = $RequireDrivers -or ([version]$Version -ge [version]'0.4.0')
    if ($DriversRequired -and $AllowMissingDrivers) {
        throw 'This release requires both embedded driver ZIPs; -AllowMissingDrivers is only for historical versions before 0.4.0.'
    }
    $DriverStatus = [ordered]@{}
    $DriverPackageFiles = [ordered]@{}
    foreach ($Name in @('ch343', 'cp210x')) {
        $Path = Join-Path 'assets\drivers' "$Name.zip"
        if ($FetchDrivers -and -not (Test-Path -LiteralPath $Path)) {
            & (Join-Path $PSScriptRoot 'fetch-drivers.ps1') -Driver $Name
            # Download failures throw, never becoming a successful embedded-driver build.
        }
        $Present = (Test-Path -LiteralPath $Path -PathType Leaf) -and (Get-Item -LiteralPath $Path).Length -gt 0
        if (-not $Present -and ($FetchDrivers -or $DriversRequired)) {
            throw "Missing $Path. The requested driver bundle cannot be built."
        }
        $DriverStatus[$Name] = if ($Present) { 'embedded' } else { 'not_embedded' }
        if ($Present) {
            $DriverPackageFiles[$Name] = [ordered]@{
                file = "$Name.zip"
                bytes = (Get-Item -LiteralPath $Path).Length
                sha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
                installation = 'Windows PnPUtil using the embedded complete INF/CAT/SYS package'
            }
        }
        if (-not $Present) {
            Write-Warning "$Name is NOT embedded. The user must select an official driver ZIP when installing."
        }
    }

    Invoke-Checked cargo @('fmt', '--all', '--', '--check')
    Invoke-Checked cargo @('clippy', '--locked', '--all-targets', '--target', 'x86_64-pc-windows-msvc', '--', '-D', 'warnings')
    Invoke-Checked cargo @('test', '--locked', '--all-targets', '--target', 'x86_64-pc-windows-msvc')
    Invoke-Checked cargo @('build', '--locked', '--release', '--target', 'x86_64-pc-windows-msvc')
    $Binary = Join-Path 'target\x86_64-pc-windows-msvc\release' ($Application[0].name + '.exe')
    $ReportedVersion = & $Binary --version
    if ($LASTEXITCODE -ne 0) {
        $script:NativeExitCode = $LASTEXITCODE
        throw 'The built executable failed its --version check.'
    }
    if (($ReportedVersion -join "`n") -notmatch ('(?<![0-9.])v?' + [regex]::Escape($Version) + '(?![0-9.])')) {
        throw "The executable did not report the Cargo.toml version $Version."
    }

    # Package an explicit allowlist, never the checkout, attachments, firmware or logs.
    $Staging = Join-Path ([IO.Path]::GetTempPath()) ('flashtool-package-' + [Guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($Staging)
    Copy-Item -LiteralPath $Binary -Destination (Join-Path $Staging 'FlashTool.exe')
    foreach ($Name in @('README.md', 'CHANGELOG.md')) {
        Copy-Item -LiteralPath $Name -Destination $Staging
    }
    foreach ($File in Get-ChildItem -File | Where-Object { $_.Name -match '^(LICENSE|COPYING|NOTICE|THIRD_PARTY)' }) {
        Copy-Item -LiteralPath $File.FullName -Destination $Staging
    }
    $DriverDocumentation = Join-Path $Staging 'drivers'
    [void][IO.Directory]::CreateDirectory($DriverDocumentation)
    Copy-Item -LiteralPath assets\drivers\README.md -Destination $DriverDocumentation
    foreach ($Name in @('provenance.json', 'cp210x-license.txt')) {
        $Notice = Join-Path 'assets\drivers' $Name
        if (Test-Path -LiteralPath $Notice -PathType Leaf) {
            Copy-Item -LiteralPath $Notice -Destination $DriverDocumentation
        }
    }
    foreach ($Name in @('ch343', 'cp210x')) {
        if ($DriverStatus[$Name] -eq 'embedded') {
            foreach ($Suffix in @('source.json', 'sha256')) {
                $Record = Join-Path 'assets\drivers' "$Name.$Suffix"
                if (Test-Path -LiteralPath $Record) {
                    Copy-Item -LiteralPath $Record -Destination $DriverDocumentation
                }
            }
        }
    }
    [ordered]@{
        version = $Version
        target = 'x86_64-pc-windows-msvc'
        driver_packages = $DriverStatus
        driver_package_files = $DriverPackageFiles
    } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $Staging 'BUILD-METADATA.json') -Encoding UTF8

    # Retain original dependency notices, including bundled font licences.
    $Licences = Join-Path $Staging 'third-party-licenses'
    [void][IO.Directory]::CreateDirectory($Licences)
    $DependencyRecords = @()
    foreach ($Package in $Metadata.packages) {
        if ($Package.id -eq $Application[0].id) { continue }
        $Source = Split-Path -Parent $Package.manifest_path
        $Target = Join-Path $Licences ($Package.name + '-' + $Package.version)
        $Files = @(Get-ChildItem -LiteralPath $Source -File -Recurse | Where-Object {
            $_.Name -match '(?i)(licen[cs]e|copying|notice|^OFL\.|^UFL\.|^Hack-Regular\.txt$)'
        })
        if ($Package.license_file) {
            $Explicit = Join-Path $Source $Package.license_file
            if (Test-Path -LiteralPath $Explicit -PathType Leaf) {
                $Files += Get-Item -LiteralPath $Explicit
            }
        }
        foreach ($File in ($Files | Sort-Object FullName -Unique)) {
            $Relative = $File.FullName.Substring($Source.Length).TrimStart([char[]]'\/')
            $Destination = Join-Path $Target $Relative
            [void][IO.Directory]::CreateDirectory((Split-Path -Parent $Destination))
            Copy-Item -LiteralPath $File.FullName -Destination $Destination
        }
        $DependencyRecords += [ordered]@{
            name = $Package.name; version = $Package.version
            license = $Package.license; repository = $Package.repository
        }
    }
    $DependencyRecords | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $Licences 'dependencies.json') -Encoding UTF8

    [void][IO.Directory]::CreateDirectory((Join-Path (Get-Location) 'dist'))
    $ArchiveName = "FlashTool-v$Version-windows-x64.zip"
    $ArchivePath = Join-Path 'dist' $ArchiveName
    Compress-Archive -Path (Join-Path $Staging '*') -DestinationPath $ArchivePath -Force
    $Hash = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    "$Hash  $ArchiveName" | Set-Content -LiteralPath dist\SHA256SUMS -Encoding ASCII
    Write-Host "Created $ArchivePath (SHA256 $Hash). Driver status: $($DriverStatus | ConvertTo-Json -Compress)"
    Write-Host 'No device was flashed and no driver was installed by this script.'
}
catch {
    Write-Error -ErrorRecord $_ -ErrorAction Continue
    $FailureCode = if ($script:NativeExitCode -ne 0) { $script:NativeExitCode } else { 1 }
}
finally {
    if ($Staging -and (Test-Path -LiteralPath $Staging)) {
        Remove-Item -LiteralPath $Staging -Recurse -Force
    }
    Pop-Location
}
if ($FailureCode -ne 0) { exit $FailureCode }
