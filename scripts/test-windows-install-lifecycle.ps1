<#
.SYNOPSIS
Exercises Windows NSIS install, coexistence, uninstall, and reinstall behavior.

.DESCRIPTION
All installation and application-data paths are isolated below WorkDir. Supplying
LegacyInstaller enables the coexistence assertions. Without it the script emits
SKIP and returns successfully so local Next-only validation remains useful; the
release workflow deliberately rejects that configuration before invoking it.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$NextInstaller,
    [ValidateScript({ [string]::IsNullOrWhiteSpace($_) -or (Test-Path -LiteralPath $_ -PathType Leaf) })]
    [string]$LegacyInstaller,
    [string]$WorkDir = (Join-Path ([System.IO.Path]::GetTempPath()) ('weiback-install-lifecycle-' + [guid]::NewGuid().ToString('N'))),
    [switch]$Smoke
)

$ErrorActionPreference = 'Stop'
$identity = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'release-identity.json') -Raw | ConvertFrom-Json
$previousAppData = $env:APPDATA
$previousLocalAppData = $env:LOCALAPPDATA
$nextRoot = Join-Path $WorkDir 'next-install'
$legacyRoot = Join-Path $WorkDir 'legacy-install'
$nextSentinel = Join-Path $WorkDir 'appdata\local\weiback-next\lifecycle-next.sentinel'
$legacySentinel = Join-Path $WorkDir 'appdata\local\weiback\lifecycle-legacy.sentinel'

function Fail([string]$Message) { throw "INSTALL LIFECYCLE FAILED: $Message" }

function Stop-BoundedProcess([System.Diagnostics.Process]$Process) {
    if ($null -ne $Process -and -not $Process.HasExited) {
        $Process.Kill()
        if (-not $Process.WaitForExit(5000)) { Fail "Process $($Process.Id) did not exit after kill." }
    }
}

function Invoke-BoundedProcess([string]$FileName, [string[]]$Arguments, [string]$Operation, [int]$TimeoutSeconds = 120) {
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo.FileName = $FileName
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.Arguments = (($Arguments | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + $_.Replace('"', '\"') + '"' } else { $_ }
    }) -join ' ')
    if (-not $process.Start()) { Fail "Could not start ${Operation}: $FileName" }
    try {
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            Stop-BoundedProcess $process
            Fail "$Operation timed out after $TimeoutSeconds seconds."
        }
        if ($process.ExitCode -ne 0) { Fail "$Operation failed with exit code $($process.ExitCode)." }
    } finally {
        Stop-BoundedProcess $process
        $process.Dispose()
    }
}

function Install-Nsis([string]$Installer, [string]$Destination, [string]$Label) {
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    # NSIS requires /D to be the final, unquoted command-line argument.
    Invoke-BoundedProcess $Installer @('/S', "/D=$Destination") "$Label NSIS install"
}

function Get-OnlyFile([string]$Root, [string]$Filter, [string]$Label) {
    $matches = @(Get-ChildItem -LiteralPath $Root -File -Recurse -Filter $Filter -ErrorAction SilentlyContinue)
    if ($matches.Count -ne 1) { Fail "$Label must contain exactly one $Filter file; found $($matches.Count)." }
    return $matches[0]
}

function Assert-NextBundle([string]$Root) {
    $main = Get-OnlyFile $Root "$($identity.mainBinaryName).exe" 'Next installation'
    $sidecar = Get-OnlyFile $Root "$($identity.sidecarBaseName)*.exe" 'Next installation'
    if ($main.DirectoryName -ne $sidecar.DirectoryName) { Fail 'Next main executable and sidecar are not colocated.' }
    $uninstaller = Get-OnlyFile $Root 'uninstall.exe' 'Next installation'
    return [ordered]@{ main = $main; sidecar = $sidecar; uninstaller = $uninstaller }
}

function Assert-LegacyBundle([string]$Root) {
    $uninstaller = Get-OnlyFile $Root 'uninstall.exe' 'Legacy installation'
    $executables = @(Get-ChildItem -LiteralPath $Root -File -Recurse -Filter '*.exe' | Where-Object { $_.Name -ne 'uninstall.exe' })
    if ($executables.Count -eq 0) { Fail 'Legacy installation does not contain an application executable.' }
    return [ordered]@{ main = $executables[0]; uninstaller = $uninstaller }
}

function Invoke-OptionalSmoke([System.IO.FileInfo]$Executable, [string]$Label) {
    if (-not $Smoke) { return }
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo.FileName = $Executable.FullName
    $process.StartInfo.UseShellExecute = $false
    if (-not $process.Start()) { Fail "Could not start $Label smoke process." }
    try {
        Start-Sleep -Seconds 3
        if ($process.HasExited -and $process.ExitCode -ne 0) { Fail "$Label smoke process exited with code $($process.ExitCode)." }
    } finally {
        Stop-BoundedProcess $process
        $process.Dispose()
    }
}

New-Item -ItemType Directory -Force -Path $WorkDir, (Split-Path -Parent $nextSentinel), (Split-Path -Parent $legacySentinel) | Out-Null
$env:APPDATA = Join-Path $WorkDir 'appdata\roaming'
$env:LOCALAPPDATA = Join-Path $WorkDir 'appdata\local'
New-Item -ItemType Directory -Force -Path $env:APPDATA, $env:LOCALAPPDATA | Out-Null

try {
    Install-Nsis $NextInstaller $nextRoot 'Next'
    $next = Assert-NextBundle $nextRoot
    Set-Content -LiteralPath $nextSentinel -Value 'next-data-must-survive-uninstall-and-reinstall' -Encoding utf8
    Invoke-OptionalSmoke $next.main 'Next'
    Invoke-OptionalSmoke $next.sidecar 'Next sidecar'

    if ([string]::IsNullOrWhiteSpace($LegacyInstaller)) {
        Write-Host "SKIP: coexistence, legacy preservation, and reinstall checks require -LegacyInstaller. Next install verified at $nextRoot"
        return
    }

    Install-Nsis $LegacyInstaller $legacyRoot 'Legacy'
    $legacy = Assert-LegacyBundle $legacyRoot
    Set-Content -LiteralPath $legacySentinel -Value 'legacy-data-must-survive-next-uninstall' -Encoding utf8
    Invoke-OptionalSmoke $legacy.main 'Legacy'
    if ($next.main.FullName -eq $legacy.main.FullName -or $nextRoot -eq $legacyRoot) { Fail 'Next and Legacy were not installed side by side.' }
    if (-not (Test-Path -LiteralPath $nextSentinel) -or -not (Test-Path -LiteralPath $legacySentinel)) { Fail 'Coexistence sentinel is missing.' }

    Invoke-BoundedProcess $next.uninstaller.FullName @('/S') 'Next NSIS uninstall'
    if (Test-Path -LiteralPath $next.main.FullName) { Fail 'Next executable remains after uninstall.' }
    if (-not (Test-Path -LiteralPath $legacy.main.FullName) -or -not (Test-Path -LiteralPath $legacySentinel)) { Fail 'Uninstalling Next changed the Legacy installation or data.' }

    Install-Nsis $NextInstaller $nextRoot 'Next reinstall'
    $next = Assert-NextBundle $nextRoot
    if (-not (Test-Path -LiteralPath $nextSentinel)) { Fail 'Next data sentinel was removed by uninstall or reinstall.' }
    if (-not (Test-Path -LiteralPath $legacy.main.FullName) -or -not (Test-Path -LiteralPath $legacySentinel)) { Fail 'Reinstalling Next changed Legacy state.' }
    Write-Host "PASS: Next/Legacy install lifecycle verified in $WorkDir"
} finally {
    try {
        if (Test-Path -LiteralPath $nextRoot) {
            $uninstaller = @(Get-ChildItem -LiteralPath $nextRoot -File -Recurse -Filter 'uninstall.exe' -ErrorAction SilentlyContinue)[0]
            if ($null -ne $uninstaller) { Invoke-BoundedProcess $uninstaller.FullName @('/S') 'Next cleanup uninstall' }
        }
        if (Test-Path -LiteralPath $legacyRoot) {
            $uninstaller = @(Get-ChildItem -LiteralPath $legacyRoot -File -Recurse -Filter 'uninstall.exe' -ErrorAction SilentlyContinue)[0]
            if ($null -ne $uninstaller) { Invoke-BoundedProcess $uninstaller.FullName @('/S') 'Legacy cleanup uninstall' }
        }
    } finally {
        $env:APPDATA = $previousAppData
        $env:LOCALAPPDATA = $previousLocalAppData
    }
}
