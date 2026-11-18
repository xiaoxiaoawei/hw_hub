# Download and inspect official driver packages. This script never installs them.
[CmdletBinding()]
param(
    [ValidateSet('all', 'ch343', 'cp210x')]
    [string]$Driver = 'all',
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\assets\drivers'),
    [ValidatePattern('^$|^[0-9a-fA-F]{64}$')]
    [string]$ExpectedSha256 = '',
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($Driver -eq 'all' -and $ExpectedSha256) {
    throw 'Use -Driver ch343 or -Driver cp210x with -ExpectedSha256.'
}

# Preserve OS certificate verification; older Windows PowerShell may need TLS 1.2.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
Add-Type -AssemblyName System.IO.Compression.FileSystem
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
[void][IO.Directory]::CreateDirectory($OutputDirectory)

$Packages = @{
    ch343 = @{
        Url = 'https://dl.espressif.com/dl/idf-installer/CH341SER.ZIP'
        VendorPage = 'https://www.wch.cn/downloads/CH343SER_ZIP.html'
        Inf = 'CH343SER.INF'
        HardwarePattern = 'USB\\VID_1A86&PID_55D[34]'
    }
    cp210x = @{
        Url = 'https://dl.espressif.com/dl/idf-installer/CP210x_Universal_Windows_Driver.zip'
        VendorPage = 'https://www.silabs.com/developers/usb-to-uart-bridge-vcp-drivers'
        Inf = 'silabser.inf'
        HardwarePattern = 'USB\\VID_10C4&PID_EA60'
    }
}

function Test-DriverArchive {
    param([string]$Path, [hashtable]$Package)
    $Archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        if ($Archive.Entries.Count -eq 0 -or $Archive.Entries.Count -gt 4096) {
            throw 'Unexpected driver ZIP entry count.'
        }
        [long]$Total = 0
        foreach ($Entry in $Archive.Entries) {
            $Name = $Entry.FullName.Replace('\', '/')
            if ($Name.StartsWith('/') -or $Name.Contains(':') -or $Name -match '(^|/)\.\.(/|$)') {
                throw 'The ZIP contains an unsafe entry name.'
            }
            $Total += $Entry.Length
            if ($Entry.Length -gt 64MB -or $Total -gt 128MB) {
                throw 'The driver ZIP exceeds the extraction size limit.'
            }
        }
        $Infs = @($Archive.Entries | Where-Object {
            [IO.Path]::GetFileName($_.FullName.Replace('\', '/')).Equals($Package.Inf, [StringComparison]::OrdinalIgnoreCase)
        })
        if ($Infs.Count -ne 1) {
            throw "Expected exactly one $($Package.Inf); found $($Infs.Count)."
        }
        $Reader = [IO.StreamReader]::new($Infs[0].Open(), [Text.Encoding]::UTF8, $true)
        try { $InfText = $Reader.ReadToEnd() } finally { $Reader.Dispose() }
        $ActiveLines = @($InfText -split '\r?\n' | ForEach-Object { ($_ -split ';', 2)[0].Trim() })
        $MatchingModel = @($ActiveLines | Where-Object { $_.Contains('=') -and $_ -match $Package.HardwarePattern })
        if ($MatchingModel.Count -eq 0) { throw 'The driver INF does not contain the expected USB hardware ID.' }
        $Catalogs = @($Archive.Entries | Where-Object { $_.FullName -match '(?i)\.cat$' -and $_.Length -gt 0 })
        $Systems = @($Archive.Entries | Where-Object { $_.FullName -match '(?i)\.sys$' -and $_.Length -gt 0 })
        if ($Catalogs.Count -eq 0 -or $Systems.Count -eq 0) {
            throw 'The ZIP is missing its CAT signature catalog or SYS driver files.'
        }
        return @($Archive.Entries | Where-Object {
            $_.FullName -match '(?i)(licen[cs]e|eula|copying|readme|release.?notes)'
        } | ForEach-Object { $_.FullName })
    }
    finally { $Archive.Dispose() }
}

$Selected = if ($Driver -eq 'all') { @('ch343', 'cp210x') } else { @($Driver) }
foreach ($Name in $Selected) {
    $Package = $Packages[$Name]
    $Destination = Join-Path $OutputDirectory "$Name.zip"
    if ((Test-Path -LiteralPath $Destination) -and -not $Force) {
        throw "$Destination already exists. Preserve it or explicitly use -Force to replace it."
    }
    $Temporary = Join-Path $OutputDirectory ('.' + $Name + '-' + [Guid]::NewGuid().ToString('N') + '.partial')
    try {
        Write-Host "Downloading $Name from $($Package.Url)"
        Invoke-WebRequest -Uri $Package.Url -OutFile $Temporary -UseBasicParsing
        $Size = (Get-Item -LiteralPath $Temporary).Length
        if ($Size -le 0 -or $Size -gt 64MB) { throw 'The downloaded ZIP is empty or exceeds 64 MiB.' }
        $Hash = (Get-FileHash -LiteralPath $Temporary -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($ExpectedSha256 -and $Hash -ne $ExpectedSha256.ToLowerInvariant()) {
            throw "SHA256 mismatch for $Name. The package was not saved."
        }
        $Notices = @(Test-DriverArchive -Path $Temporary -Package $Package)
        Move-Item -LiteralPath $Temporary -Destination $Destination -Force:$Force
        $Record = [ordered]@{
            name = $Name
            filename = "$Name.zip"
            sha256 = $Hash
            bytes = $Size
            source = $Package.Url
            vendor_page = $Package.VendorPage
            downloaded_at_utc = [DateTime]::UtcNow.ToString('o')
            expected_hash_was_supplied = [bool]$ExpectedSha256
            notice_entries = $Notices
            redistribution_terms_reviewed = $false
            note = 'The archive is preserved in full. Windows PnPUtil validates package signatures at installation time.'
        }
        $Record | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Name.source.json") -Encoding UTF8
        "$Hash  $Name.zip" | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Name.sha256") -Encoding UTF8
        Write-Host "Saved $Name.zip (SHA256 $Hash). No driver has been installed."
        Write-Host 'Review the vendor redistribution terms before publishing an executable that embeds this ZIP.'
    }
    finally {
        if (Test-Path -LiteralPath $Temporary) { Remove-Item -LiteralPath $Temporary }
    }
}
