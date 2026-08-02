[CmdletBinding()]
param(
    [string]$ReleaseDir = (Join-Path $PSScriptRoot '..\tauri-app\src-tauri\target\release'),
    [string]$WorkDir = (Join-Path ([System.IO.Path]::GetTempPath()) ('weiback-release-candidate-' + [guid]::NewGuid().ToString('N')))
)

$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'verify-release.ps1') -Mode candidate -ReleaseDir $ReleaseDir -WorkDir $WorkDir -AllowMissingArtifacts
exit $LASTEXITCODE
