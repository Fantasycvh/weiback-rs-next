<#
.SYNOPSIS
Validates a WeiBack Next Windows release without altering the release artifacts.

.DESCRIPTION
Candidate mode permits unsigned artifacts but always reports their signature state.
Release mode requires Valid Authenticode signatures for the main executable,
sidecar, MSI, and NSIS installer. All reports and installer extraction happen in
the caller-supplied working directory.
#>
[CmdletBinding()]
param(
    [ValidateSet('candidate', 'release')]
    [string]$Mode = 'candidate',
    [string]$ReleaseDir = (Join-Path $PSScriptRoot '..\tauri-app\src-tauri\target\release'),
    [string]$WorkDir = (Join-Path ([System.IO.Path]::GetTempPath()) ('weiback-release-verify-' + [guid]::NewGuid().ToString('N'))),
    [switch]$AllowMissingArtifacts,
    [switch]$SkipInstallerExtraction,
    [string]$TestSignatureReportPath
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$identity = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'release-identity.json') -Raw | ConvertFrom-Json
$requiredArtifactNames = @('main executable', 'sidecar', 'MSI installer', 'NSIS installer')

function Fail([string]$Message) { throw "RELEASE GATE FAILED: $Message" }

function New-ProtocolUuidV7 {
    $random = New-Object byte[] 10
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($random)
    $randomHex = ([BitConverter]::ToString($random)).Replace('-', '').ToLowerInvariant()
    $timestamp = '{0:x12}' -f [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $variant = '{0:x}' -f (8 + ($random[1] -band 0x03))
    '{0}-{1}-7{2}-{3}{4}-{5}' -f $timestamp.Substring(0, 8), $timestamp.Substring(8, 4), $randomHex.Substring(0, 3), $variant, $randomHex.Substring(3, 3), $randomHex.Substring(6, 12)
}

function Read-Json([string]$Path) {
    try { return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json }
    catch { Fail "Invalid JSON: $Path. $($_.Exception.Message)" }
}

function Get-SingleVersion([string]$Name, [string]$Path, [scriptblock]$Reader) {
    if (-not (Test-Path -LiteralPath $Path)) { Fail "$Name version source is missing: $Path" }
    $value = & $Reader
    if ([string]::IsNullOrWhiteSpace($value)) { Fail "$Name version is empty: $Path" }
    return [string]$value
}

function Assert-VersionConsistency {
    $rootCargo = Join-Path $repoRoot 'Cargo.toml'
    $tauriConfig = Join-Path $repoRoot 'tauri-app\src-tauri\tauri.conf.json'
    $packageJson = Join-Path $repoRoot 'tauri-app\package.json'
    $cargoVersion = Get-SingleVersion 'workspace Cargo.toml' $rootCargo {
        if ((Get-Content -LiteralPath $rootCargo -Raw) -match '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') { $Matches[1] }
    }
    $tauriVersion = Get-SingleVersion 'tauri.conf.json' $tauriConfig { (Read-Json $tauriConfig).version }
    $packageVersion = Get-SingleVersion 'package.json' $packageJson { (Read-Json $packageJson).version }
    if (($cargoVersion, $tauriVersion, $packageVersion | Select-Object -Unique).Count -ne 1) {
        Fail "Version mismatch: Cargo=$cargoVersion; tauri=$tauriVersion; package=$packageVersion"
    }
    return $cargoVersion
}

function Assert-StaticIdentity {
    $config = Read-Json (Join-Path $repoRoot 'tauri-app\src-tauri\tauri.conf.json')
    if ($config.productName -ne $identity.productName) { Fail "productName must remain $($identity.productName)" }
    if ($config.identifier -ne $identity.identifier) { Fail "Tauri identifier must remain $($identity.identifier)" }
    if ($config.mainBinaryName -ne $identity.mainBinaryName) { Fail "mainBinaryName must remain $($identity.mainBinaryName)" }
    if ($config.bundle.windows.nsis.installMode -ne $identity.nsisInstallMode) { Fail "NSIS installMode must remain $($identity.nsisInstallMode)" }
    if ($config.bundle.externalBin -notcontains "binaries/$($identity.sidecarBaseName)") { Fail 'Tauri externalBin must package the collector sidecar.' }
    $configRs = Get-Content -LiteralPath (Join-Path $repoRoot 'weiback\src\config.rs') -Raw
    if ($configRs -notmatch ('APP_NAMESPACE:\s*&str\s*=\s*"' + [regex]::Escape($identity.dataNamespace) + '"')) { Fail 'Runtime data namespace differs from release identity baseline.' }
    if ($configRs -match ('APP_NAMESPACE:\s*&str\s*=\s*"' + [regex]::Escape($identity.legacyIdentifier) + '"')) { Fail 'Runtime data namespace points at the legacy application.' }
    $tauriCli = Join-Path $repoRoot 'tauri-app\node_modules\.bin\tauri.cmd'
    if (Test-Path -LiteralPath $tauriCli) {
        $previousErrorPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $upgradeCodeOutput = @(& $tauriCli inspect wix-upgrade-code 2>&1 | ForEach-Object { $_.ToString() })
        $ErrorActionPreference = $previousErrorPreference
        $upgradeCode = @($upgradeCodeOutput | Where-Object { $_ -match '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' } | Select-Object -Last 1)[0]
        if ($LASTEXITCODE -ne 0) { Fail "Could not inspect the WiX UpgradeCode (exit code $LASTEXITCODE)." }
        if ([string]::IsNullOrWhiteSpace($upgradeCode)) { Fail 'Tauri did not return a WiX UpgradeCode.' }
        if ($upgradeCode -notmatch [regex]::Escape($identity.wixUpgradeCode)) { Fail "WiX UpgradeCode differs from identity baseline $($identity.wixUpgradeCode)." }
    }
}

function Find-Artifact([string]$Label, [string[]]$Patterns) {
    if (-not (Test-Path -LiteralPath $ReleaseDir)) { return $null }
    $matches = foreach ($pattern in $Patterns) {
        Get-ChildItem -LiteralPath $ReleaseDir -File -Recurse -Filter $pattern -ErrorAction SilentlyContinue
    }
    $matches = @($matches | Sort-Object FullName -Unique)
    if ($matches.Count -eq 0) { return $null }
    if ($matches.Count -gt 1) { Fail "Multiple $Label candidates found: $($matches.FullName -join '; ')" }
    return $matches[0]
}

function Get-SignatureReport([System.IO.FileInfo]$File) {
    if (-not [string]::IsNullOrWhiteSpace($TestSignatureReportPath)) {
        if ($env:WEIBACK_RELEASE_TESTING -ne '1') { Fail 'TestSignatureReportPath is only available when WEIBACK_RELEASE_TESTING=1.' }
        $allReports = @()
        foreach ($candidate in @(Read-Json $TestSignatureReportPath)) {
            if ($candidate -is [array]) { $allReports += $candidate } else { $allReports += $candidate }
        }
        $report = @($allReports | Where-Object { $_.path -eq $File.FullName })
        if ($report.Count -ne 1) { Fail "Test signature report must contain exactly one entry for $($File.FullName)." }
        $entry = $report[0]
        return [pscustomobject]@{ path = $File.FullName; status = [string]($entry.status); thumbprint = [string]($entry.thumbprint); signer = [string]($entry.subject) }
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $File.FullName
    [pscustomobject]@{ path = $File.FullName; status = [string]$signature.Status; thumbprint = if ($signature.SignerCertificate) { $signature.SignerCertificate.Thumbprint } else { $null }; signer = if ($signature.SignerCertificate) { $signature.SignerCertificate.Subject } else { $null } }
}

function Get-ApprovedSignerPolicy {
    if ($null -eq $identity.approvedSigners) { Fail 'release-identity.json must define approvedSigners.' }
    $thumbprints = @($identity.approvedSigners.thumbprints | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    $subjectRegexes = @($identity.approvedSigners.subjectRegexes | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if (($thumbprints.Count + $subjectRegexes.Count) -eq 0) { Fail 'Release signer policy must contain at least one approved thumbprint or subject regex.' }
    if (-not [string]::IsNullOrWhiteSpace($env:WEIBACK_APPROVED_SIGNER_THUMBPRINTS)) {
        $thumbprints = @($env:WEIBACK_APPROVED_SIGNER_THUMBPRINTS -split '[,;\s]+' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    }
    return [ordered]@{ thumbprints = $thumbprints; subjectRegexes = $subjectRegexes }
}

function Test-ApprovedSigner($Report, $Policy) {
    $thumbprint = ([string]$Report.thumbprint).Replace(' ', '')
    if ($Policy.thumbprints -contains $thumbprint) { return $true }
    return @($Policy.subjectRegexes | Where-Object { $Report.signer -match $_ }).Count -gt 0
}

function Assert-Signatures([System.IO.FileInfo[]]$Files) {
    $reports = @($Files | ForEach-Object { Get-SignatureReport $_ })
    if ($Mode -eq 'release') {
        $invalid = @($reports | Where-Object { $_.status -ne 'Valid' })
        if ($invalid.Count -gt 0) { Fail "Release mode requires Valid Authenticode signatures: $(($invalid | ForEach-Object { "$($_.path)=$($_.status)" }) -join '; ')" }
        $policy = Get-ApprovedSignerPolicy
        $unapproved = @($reports | Where-Object { -not (Test-ApprovedSigner $_ $policy) })
        if ($unapproved.Count -gt 0) { Fail "Release signature signer is not approved: $(($unapproved | ForEach-Object { "$($_.path)=$($_.signer) [$($_.thumbprint)]" }) -join '; ')" }
    }
    return $reports
}

function Stop-ReleaseProcess([System.Diagnostics.Process]$Process) {
    if (-not $Process.HasExited) {
        $Process.Kill()
        [void]$Process.WaitForExit(5000)
    }
}

function Start-InstallerProcess([string]$FileName, [string[]]$Arguments, [string]$Operation, [int]$TimeoutSeconds = 120) {
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo.FileName = $FileName
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.Arguments = (($Arguments | ForEach-Object { if ($_ -match '[\s"]') { '"' + $_.Replace('"', '\"') + '"' } else { $_ } }) -join ' ')
    if (-not $process.Start()) { Fail "Could not start ${Operation}: $FileName" }
    try {
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            Stop-ReleaseProcess $process
            Fail "$Operation timed out after $TimeoutSeconds seconds."
        }
        if ($process.ExitCode -ne 0) { Fail "$Operation failed with exit code $($process.ExitCode)." }
    } finally {
        Stop-ReleaseProcess $process
        $process.Dispose()
    }
}

function Invoke-SidecarProtocol([System.IO.FileInfo]$Sidecar) {
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo.FileName = $Sidecar.FullName
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.RedirectStandardInput = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    $process.StartInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    if (-not $process.Start()) { Fail "Could not start sidecar: $($Sidecar.FullName)" }
    try {
        $helloId = New-ProtocolUuidV7
        $shutdownId = New-ProtocolUuidV7
        $input = @(
            (@{ protocol_version = 1; request_id = $helloId; type = 'hello'; payload = @{} } | ConvertTo-Json -Compress),
            (@{ protocol_version = 1; request_id = $shutdownId; type = 'shutdown'; payload = @{ grace_ms = 0 } } | ConvertTo-Json -Compress)
        ) -join "`n"
        $inputBytes = [Text.UTF8Encoding]::new($false).GetBytes("$input`n")
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.StandardInput.BaseStream.Write($inputBytes, 0, $inputBytes.Length)
        $process.StandardInput.BaseStream.Flush()
        $process.StandardInput.Close()
        if (-not $process.WaitForExit(15000)) {
            Stop-ReleaseProcess $process
            $stderr = if ($stderrTask.Wait(5000)) { $stderrTask.Result } else { '' }
            Fail "Sidecar did not exit after hello and shutdown within 15 seconds. stderr: $stderr"
        }
        if (-not $stdoutTask.Wait(5000) -or -not $stderrTask.Wait(5000)) {
            Fail 'Sidecar output drain did not complete within 5 seconds.'
        }
        $events = @($stdoutTask.Result -split "`r?`n" | Where-Object { $_.Trim() } | ForEach-Object { $_ | ConvertFrom-Json })
        $ready = @($events | Where-Object { $_.type -eq 'ready' })[0]
        $capabilities = @($events | Where-Object { $_.type -eq 'capabilities' })[0]
        if ($null -eq $ready -or $ready.payload.sidecar_name -ne $identity.sidecarBaseName -or $ready.payload.protocol_version -ne 1) { Fail 'Sidecar hello returned an invalid ready event.' }
        if ($null -eq $capabilities -or $capabilities.payload.commands -notcontains 'shutdown') { Fail 'Sidecar hello did not advertise shutdown.' }
        if ($process.ExitCode -ne 0) { Fail "Sidecar shutdown exit code was $($process.ExitCode)." }
        return [ordered]@{ sidecar_name = $ready.payload.sidecar_name; sidecar_version = $ready.payload.sidecar_version; protocol_version = $ready.payload.protocol_version; shutdown_exit_code = $process.ExitCode }
    } finally {
        Stop-ReleaseProcess $process
        $process.Dispose()
    }
}

function Assert-ExtractedBundle([string]$Kind, [string]$Root, [string]$ExpectedSidecarHash) {
    $mainCandidates = @(Get-ChildItem -LiteralPath $Root -File -Recurse -Filter "$($identity.mainBinaryName).exe")
    $sidecarCandidates = @(Get-ChildItem -LiteralPath $Root -File -Recurse -Filter "$($identity.sidecarBaseName)*.exe")
    if ($mainCandidates.Count -ne 1 -or $sidecarCandidates.Count -ne 1) { Fail "$Kind extraction must contain exactly one main executable and one sidecar. Found main=$($mainCandidates.Count), sidecar=$($sidecarCandidates.Count)." }
    $main = $mainCandidates[0]
    $sidecar = $sidecarCandidates[0]
    if ([System.IO.Path]::GetDirectoryName($main.FullName) -ne [System.IO.Path]::GetDirectoryName($sidecar.FullName)) { Fail "$Kind extraction has an unexpected main executable/sidecar layout." }
    $actualHash = (Get-FileHash -LiteralPath $sidecar.FullName -Algorithm SHA256).Hash
    if ($actualHash -ne $ExpectedSidecarHash) { Fail "$Kind extraction sidecar hash differs from the release sidecar." }
    if ($Mode -eq 'release') { [void](Assert-Signatures @($main, $sidecar)) }
    return [ordered]@{ kind = $Kind; main = $main.FullName; sidecar = $sidecar.FullName; sidecar_sha256 = $actualHash }
}

function Invoke-InstallerExtraction([System.IO.FileInfo]$Msi, [System.IO.FileInfo]$Nsis, [string]$ExpectedSidecarHash) {
    if ($SkipInstallerExtraction) { return @([ordered]@{ skipped = $true; reason = 'SkipInstallerExtraction was explicitly supplied.' }) }
    $result = @()
    $msiRoot = Join-Path $WorkDir 'msi-admin'
    New-Item -ItemType Directory -Force -Path $msiRoot | Out-Null
    Start-InstallerProcess 'msiexec.exe' @('/a', $Msi.FullName, "TARGETDIR=$msiRoot", '/qn', '/norestart') 'MSI administrative extraction'
    $result += Assert-ExtractedBundle 'MSI' $msiRoot $ExpectedSidecarHash

    $nsisRoot = Join-Path $WorkDir 'nsis-install'
    New-Item -ItemType Directory -Force -Path $nsisRoot | Out-Null
    try {
        Start-InstallerProcess $Nsis.FullName @('/S', "/D=$nsisRoot") 'NSIS temporary installation'
        $result += Assert-ExtractedBundle 'NSIS' $nsisRoot $ExpectedSidecarHash
    } finally {
        $uninstaller = Join-Path $nsisRoot 'uninstall.exe'
        if (Test-Path -LiteralPath $uninstaller) { Start-InstallerProcess $uninstaller @('/S') 'NSIS temporary uninstall' }
    }
    return $result
}

$version = Assert-VersionConsistency
Assert-StaticIdentity
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

$main = Find-Artifact 'main executable' @("$($identity.mainBinaryName).exe")
$sidecar = Find-Artifact 'sidecar' @("$($identity.sidecarBaseName)-x86_64-pc-windows-msvc.exe", "$($identity.sidecarBaseName).exe")
$msi = Find-Artifact 'MSI installer' @('*.msi')
$nsis = Find-Artifact 'NSIS installer' @('*-setup.exe', '*_x64-setup.exe')
$artifacts = @(@($main, $sidecar, $msi, $nsis) | Where-Object { $null -ne $_ })

if ($artifacts.Count -ne 4) {
    $missing = for ($i = 0; $i -lt 4; $i++) { if ($null -eq @($main, $sidecar, $msi, $nsis)[$i]) { $requiredArtifactNames[$i] } }
    if ($Mode -eq 'release' -or -not $AllowMissingArtifacts) { Fail "Missing required release artifacts under ${ReleaseDir}: $($missing -join ', ')" }
    $report = [ordered]@{ mode = $Mode; version = $version; artifact_root = $ReleaseDir; status = 'configuration-only'; missing_artifacts = $missing; unsigned_candidate_is_allowed = ($Mode -eq 'candidate') }
    $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $WorkDir 'release-verification.json') -Encoding utf8
    Write-Host "PASS (configuration-only): version and identity checks passed; missing artifacts: $($missing -join ', ')"
    return
}

$signatureReports = Assert-Signatures $artifacts
$manifestFiles = foreach ($artifact in $artifacts) { [ordered]@{ path = $artifact.FullName.Substring((Resolve-Path -LiteralPath $ReleaseDir).Path.Length).TrimStart('\'); sha256 = (Get-FileHash -LiteralPath $artifact.FullName -Algorithm SHA256).Hash; bytes = $artifact.Length } }
$sidecarHash = @($manifestFiles | Where-Object { $_.path -match [regex]::Escape($identity.sidecarBaseName) })[0].sha256
$protocol = Invoke-SidecarProtocol $sidecar
$installers = Invoke-InstallerExtraction $msi $nsis $sidecarHash
$report = [ordered]@{ mode = $Mode; version = $version; artifact_root = (Resolve-Path -LiteralPath $ReleaseDir).Path; unsigned_candidate_is_allowed = ($Mode -eq 'candidate'); identity = $identity; signatures = $signatureReports; sidecar_protocol = $protocol; installers = $installers; artifacts = $manifestFiles }
$manifestPath = Join-Path $WorkDir 'release-manifest.json'
$report | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $manifestPath -Encoding utf8
Write-Host "PASS: $Mode release integrity verified. Manifest: $manifestPath"
