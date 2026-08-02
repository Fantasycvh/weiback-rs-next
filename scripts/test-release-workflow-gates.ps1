[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot

function Assert-WorkflowGate {
    param(
        [Parameter(Mandatory)] [string] $WorkflowName,
        [Parameter(Mandatory)] [string] $Source,
        [Parameter(Mandatory)] [string] $BuildStep,
        [Parameter(Mandatory)] [string[]] $PostGateSteps
    )

    if ($Source -notmatch "(?s)actions/setup-python@v5\s+with:\s+python-version: '3\.12'") {
        throw "$WorkflowName must use Python 3.12 for sidecar tests and packaging."
    }

    $requiredPatterns = @(
        '(?s)- name: Test Rust workspace\s+working-directory: \.\s+run: \|\s+\$env:CARGO_BUILD_JOBS = ''1''\s+cargo test --workspace',
        '(?s)- name: Lint Rust workspace\s+working-directory: \.\s+run: \|\s+\$env:CARGO_BUILD_JOBS = ''1''\s+cargo clippy --workspace --all-targets -- -D warnings',
        '(?s)- name: Test collector sidecar\s+working-directory: sidecar\s+run: \|\s+python -m pip install --disable-pip-version-check --upgrade pip\s+python -m pip install --disable-pip-version-check -e \.\s+\$env:PYTHONPATH = \(Get-Location\)\.Path\s+python -m unittest discover -s tests -t \. -v',
        '(?s)- name: Validate frontend\s+working-directory: tauri-app\s+run: \|\s+yarn build\s+yarn lint'
    )
    foreach ($pattern in $requiredPatterns) {
        if ($Source -notmatch $pattern) {
            throw "$WorkflowName is missing a required release test gate or its working directory."
        }
    }

    $orderedSteps = @('Test Rust workspace', 'Lint Rust workspace', 'Test collector sidecar', 'Validate frontend', 'Build collector sidecar', $BuildStep)
    $positions = @($orderedSteps | ForEach-Object {
        $position = $Source.IndexOf("- name: $_", [StringComparison]::Ordinal)
        if ($position -lt 0) { throw "$WorkflowName is missing step: $_" }
        $position
    })
    for ($index = 1; $index -lt $positions.Count; $index++) {
        if ($positions[$index - 1] -ge $positions[$index]) {
            throw "$WorkflowName must run all release test gates before $BuildStep."
        }
    }

    $frontendGate = $Source.IndexOf('- name: Validate frontend', [StringComparison]::Ordinal)
    foreach ($step in $PostGateSteps) {
        $position = $Source.IndexOf("- name: $step", [StringComparison]::Ordinal)
        if ($position -lt 0) { throw "$WorkflowName is missing protected step: $step" }
        if ($frontendGate -ge $position) {
            throw "$WorkflowName must run all release test gates before $step."
        }
    }

    $sidecarTest = $Source.IndexOf('- name: Test collector sidecar', [StringComparison]::Ordinal)
    $pyinstaller = $Source.IndexOf('python -m pip install --disable-pip-version-check pyinstaller', [StringComparison]::Ordinal)
    if ($pyinstaller -lt $sidecarTest) {
        throw "$WorkflowName must install project dependencies and run unittest before installing PyInstaller."
    }
}

foreach ($workflow in @(
    @{ Name = 'release.yml'; BuildStep = 'Build Windows installers without uploading'; PostGateSteps = @('Sign final Windows artifacts', 'Create draft release and upload verified Windows installers') },
    @{ Name = 'release-integrity.yml'; BuildStep = 'Build Windows installers'; PostGateSteps = @('Verify candidate artifacts', 'Verify signed release artifacts') }
)) {
    $path = Join-Path $repoRoot ".github\workflows\$($workflow.Name)"
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing workflow: $path" }
    Assert-WorkflowGate -WorkflowName $workflow.Name -Source (Get-Content -LiteralPath $path -Raw) -BuildStep $workflow.BuildStep -PostGateSteps $workflow.PostGateSteps
}

$releaseSource = Get-Content -LiteralPath (Join-Path $repoRoot '.github\workflows\release.yml') -Raw
if ($releaseSource -match 'workflow_dispatch:') {
    throw 'release.yml must not allow workflow_dispatch because only version tags may upload release assets.'
}
if ($releaseSource -notmatch "- 'v\*'") {
    throw 'release.yml must use a GitHub tag glob that matches normal vX.Y.Z tags.'
}
if ($releaseSource -match 'v\[0-9\]\+\.\*') {
    throw 'release.yml must not use regex syntax in a GitHub tag glob.'
}
if ($releaseSource -notmatch "Validate version tag before release work") {
    throw 'release.yml must reject non-version tags before checkout, tests, builds, or signing.'
}
if ($releaseSource -notmatch "\^v\(0\|\[1-9\]\\d\*\)") {
    throw 'release.yml must validate semantic version tags before release work.'
}
$semverPattern = '^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$'
foreach ($tag in @('v0.3.1', 'v1.2.3-rc.1+build.5', 'v1.2.3+build.5')) {
    if ($tag -notmatch $semverPattern) { throw "Release semver gate rejected valid tag: $tag" }
}
foreach ($tag in @('v01.2.3', 'v1.2.3-.', 'v1.2.3-', 'v1.2', 'vfoo')) {
    if ($tag -match $semverPattern) { throw "Release semver gate accepted invalid tag: $tag" }
}
if ($releaseSource -notmatch "Release upload requires a version tag") {
    throw 'release.yml must validate the tag before creating a draft release.'
}

Write-Host 'PASS: Windows release workflows run Rust, Python, and frontend gates before release builds.'
