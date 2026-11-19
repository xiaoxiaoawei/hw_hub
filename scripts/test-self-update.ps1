[CmdletBinding()]
param(
    [ValidateSet('Release', 'Debug')]
    [string]$Configuration = 'Release',
    [ValidateSet('x86_64-pc-windows-msvc', 'x86_64-pc-windows-gnu')]
    [string]$Target = 'x86_64-pc-windows-msvc',
    [ValidateRange(30, 600)]
    [int]$TimeoutSeconds = 180
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Process = $null
Push-Location (Join-Path $PSScriptRoot '..')
try {
    if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitProcess) {
        throw 'The self-update smoke requires native 64-bit Windows and PowerShell 7.'
    }
    # The example uses real production helper/ACK entry points. It creates only
    # private executable copies, a local ZIP and isolated child AppData/TEMP.
    # No network, installed FlashTool, or real user settings are involved.
    $BuildArguments = @('build', '--locked', '--target', $Target, '--example', 'self_update_smoke')
    if ($Configuration -eq 'Release') { $BuildArguments += '--release' }
    & cargo @BuildArguments
    if ($LASTEXITCODE -ne 0) { throw "Building the update smoke failed with exit code $LASTEXITCODE." }

    $MetadataText = & cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'Could not find the Cargo target directory.' }
    $Metadata = $MetadataText | ConvertFrom-Json
    $Profile = if ($Configuration -eq 'Release') { 'release' } else { 'debug' }
    $Executable = Join-Path $Metadata.target_directory "$Target\$Profile\examples\self_update_smoke.exe"
    if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) { throw "Missing smoke executable: $Executable" }

    $StartInfo = [Diagnostics.ProcessStartInfo]::new()
    $StartInfo.FileName = (Resolve-Path -LiteralPath $Executable).ProviderPath
    $StartInfo.UseShellExecute = $false
    $StartInfo.CreateNoWindow = $true
    $StartInfo.RedirectStandardOutput = $true
    $StartInfo.RedirectStandardError = $true
    [void]$StartInfo.ArgumentList.Add('--run')
    $Process = [Diagnostics.Process]::new()
    $Process.StartInfo = $StartInfo
    if (-not $Process.Start()) { throw 'Could not start the isolated update smoke.' }
    $OutputTask = $Process.StandardOutput.ReadToEndAsync()
    $ErrorTask = $Process.StandardError.ReadToEndAsync()
    $TimedOut = -not $Process.WaitForExit($TimeoutSeconds * 1000)
    if ($TimedOut) {
        # This process and its descendants are exclusively disposable fixtures.
        if (-not $Process.HasExited) { $Process.Kill($true) }
        if (-not $Process.WaitForExit(5000)) { throw 'The timed-out smoke could not be stopped.' }
    }
    if (-not [Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]@($OutputTask, $ErrorTask), 5000)) {
        throw 'The update smoke output streams did not close.'
    }
    $Output = $OutputTask.GetAwaiter().GetResult()
    $Errors = $ErrorTask.GetAwaiter().GetResult()
    if ($Output) { Write-Host $Output.TrimEnd() }
    if ($Errors) { Write-Host $Errors.TrimEnd() }
    if ($TimedOut) { throw "The self-update smoke exceeded $TimeoutSeconds seconds." }
    if ($Process.ExitCode -ne 0) { throw "The self-update smoke failed with exit code $($Process.ExitCode)." }
    Write-Host 'Native Windows self-update smoke passed.'
} finally {
    if ($null -ne $Process) { $Process.Dispose() }
    Pop-Location
}
