[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$scriptPath = Join-Path $PSScriptRoot 'test-windows-install-lifecycle.ps1'
if (-not (Test-Path -LiteralPath $scriptPath)) { throw 'RED: lifecycle script does not exist yet.' }

$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($scriptPath, [ref]$tokens, [ref]$errors)
if ($errors.Count -gt 0) { throw "Lifecycle script has PowerShell parse errors: $($errors.Message -join '; ')" }

$parameters = @($ast.ParamBlock.Parameters | ForEach-Object { $_.Name.VariablePath.UserPath })
foreach ($required in @('NextInstaller', 'LegacyInstaller', 'WorkDir')) {
    if ($parameters -notcontains $required) { throw "Lifecycle script is missing parameter: $required" }
}

$source = Get-Content -LiteralPath $scriptPath -Raw
foreach ($guard in @('Invoke-BoundedProcess', 'Stop-BoundedProcess', 'WaitForExit', '\.Kill\(', 'finally', 'APPDATA', 'LOCALAPPDATA', 'SKIP: coexistence')) {
    if ($source -notmatch $guard) { throw "Lifecycle script is missing required guard: $guard" }
}
if ($source -notmatch 'TimeoutSeconds') { throw 'Lifecycle timeout helper must expose a timeout parameter.' }
Write-Host 'PASS: lifecycle script parameters, AST, isolation, and bounded-process guards are present.'
