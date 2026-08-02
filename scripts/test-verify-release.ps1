[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$verifyScript = Join-Path $PSScriptRoot 'verify-release.ps1'
$identityPath = Join-Path $PSScriptRoot 'release-identity.json'

if (-not (Test-Path -LiteralPath $verifyScript)) {
    throw 'RED: verify-release.ps1 does not exist yet.'
}

& $verifyScript -Mode candidate -AllowMissingArtifacts
if ($LASTEXITCODE -ne 0) {
    throw "verify-release candidate test failed with exit code $LASTEXITCODE."
}

$releaseRejected = $false
try {
    & $verifyScript -Mode release -AllowMissingArtifacts
} catch {
    if ($_.Exception.Message -match 'Missing required release artifacts') {
        $releaseRejected = $true
    } else {
        throw
    }
}
if (-not $releaseRejected) {
    throw 'Release mode must reject missing artifacts even when AllowMissingArtifacts is supplied.'
}
Write-Host 'PASS: release mode rejects incomplete artifact sets.'

$source = Get-Content -LiteralPath $verifyScript -Raw
foreach ($requiredGuard in @(
    'approvedSigners',
    'ReadToEndAsync',
    'WaitForExit',
    'Multiple.*candidates',
    'Start-InstallerProcess'
)) {
    if ($source -notmatch $requiredGuard) {
        throw "Missing release-gate safeguard: $requiredGuard"
    }
}
if ($source -match '\.Peek\(' -or $source -match 'Select-Object -First') {
    throw 'Release verification must not block on Peek() or silently select the first extracted file.'
}
if ($source -match '\.StandardInputEncoding') {
    throw 'Release verification must remain compatible with Windows PowerShell 5.1 ProcessStartInfo.'
}
if ($source -notmatch 'StandardInput\.BaseStream\.Write\(\$inputBytes') {
    throw 'Release verification must write sidecar JSONL as explicit UTF-8 on Windows PowerShell 5.1.'
}
if ($source -notmatch 'function New-ProtocolUuidV7' -or $source -notmatch '\$helloId = New-ProtocolUuidV7') {
    throw 'Release verification must send UUID v7 request IDs accepted by the Sidecar protocol.'
}
Write-Host 'PASS: release verifier contains bounded async process and unique-extraction guards.'

$identity = Get-Content -LiteralPath $identityPath -Raw | ConvertFrom-Json
if ($null -eq $identity.approvedSigners -or @($identity.approvedSigners).Count -eq 0) {
    throw 'Release identity must define at least one approved signer policy.'
}

$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('weiback-release-signer-test-' + [guid]::NewGuid().ToString('N'))
$releaseDir = Join-Path $testRoot 'release'
$workDir = Join-Path $testRoot 'work'
$signaturePath = Join-Path $testRoot 'signatures.json'
New-Item -ItemType Directory -Force -Path (Join-Path $releaseDir 'bundle\msi'), (Join-Path $releaseDir 'bundle\nsis') | Out-Null
try {
    $files = @(
        (Join-Path $releaseDir "$($identity.mainBinaryName).exe"),
        (Join-Path $releaseDir "$($identity.sidecarBaseName)-x86_64-pc-windows-msvc.exe"),
        (Join-Path $releaseDir 'bundle\msi\weiback-next.msi'),
        (Join-Path $releaseDir 'bundle\nsis\weiback-next-setup.exe')
    )
    foreach ($file in $files) { [System.IO.File]::WriteAllBytes($file, [byte[]](0)) }
    @($files | ForEach-Object {
        [pscustomobject]@{ path = $_; status = 'Valid'; thumbprint = 'BAD0BAD0BAD0BAD0'; subject = 'CN=Unapproved test signer' }
    }) | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $signaturePath -Encoding utf8

    $rejected = $false
    $previousTesting = $env:WEIBACK_RELEASE_TESTING
    $env:WEIBACK_RELEASE_TESTING = '1'
    try {
        & $verifyScript -Mode release -ReleaseDir $releaseDir -WorkDir $workDir -TestSignatureReportPath $signaturePath
    } catch {
        if ($_.Exception.Message -match 'not approved') { $rejected = $true } else { throw }
    } finally {
        $env:WEIBACK_RELEASE_TESTING = $previousTesting
    }
    if (-not $rejected) { throw 'Release mode must reject a Valid signature from an unapproved signer.' }
    Write-Host 'PASS: release mode rejects an unapproved Valid signer.'
} finally {
    Remove-Item -LiteralPath $testRoot -Recurse -Force -ErrorAction SilentlyContinue
}
