param(
    [switch]$RunInstallSmoke
)

$ErrorActionPreference = 'Stop'

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..')
$packageDir = Join-Path $repoRoot 'dist/packages'
$generatedRoot = Join-Path $packageDir '.cargo-packager'

$renderedScript = Get-ChildItem -Path $generatedRoot -Recurse -Filter installer.nsi |
    Select-Object -First 1
if (-not $renderedScript) {
    throw 'Rendered installer.nsi was not found.'
}

$content = Get-Content -Raw -Path $renderedScript.FullName
if ($content -match 'MUI_PAGE_DIRECTORY') {
    throw 'The installer still contains the directory chooser page.'
}
if ($content -notmatch 'ForceCurrentUserInstallDir') {
    throw 'The installer did not include Brick''s forced install-dir section.'
}
if ($content -notmatch 'QuietUninstallString') {
    throw 'The installer did not register QuietUninstallString.'
}
if ($content -notmatch 'ALLOWDOWNGRADES "false"') {
    throw 'The installer was rendered with downgrades allowed.'
}

if (-not $RunInstallSmoke) {
    return
}
if (-not $env:CI) {
    throw 'Install smoke mutates the Windows user profile and is intended for CI only.'
}

function Normalize-PathForCompare {
    param([string]$Path)

    return [System.IO.Path]::GetFullPath($Path).TrimEnd('\')
}

$installer = Get-ChildItem -Path $packageDir -Filter '*-setup.exe' |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1
if (-not $installer) {
    throw 'NSIS setup executable was not found.'
}

$good = Join-Path $env:LOCALAPPDATA 'Brick'
$programFilesX86 = [Environment]::GetFolderPath('ProgramFilesX86')
if ([string]::IsNullOrWhiteSpace($programFilesX86)) {
    $programFilesX86 = $env:TEMP
}
$bad = Join-Path $programFilesX86 'BrickBadLocation'
$badSecond = Join-Path $env:TEMP 'Brick Second Bad Location'

$first = Start-Process -FilePath $installer.FullName -ArgumentList "/S /D=$bad" -Wait -PassThru
if ($first.ExitCode -ne 0) {
    throw "First silent installer run failed with exit code $($first.ExitCode)."
}
if (-not (Test-Path (Join-Path $good 'brick.exe'))) {
    throw 'Brick was not installed under LOCALAPPDATA.'
}
if (Test-Path (Join-Path $bad 'brick.exe')) {
    throw 'Brick incorrectly installed into the first alternate /D path.'
}

$regPath = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Brick'
$reg = Get-ItemProperty -Path $regPath
$installLocation = ($reg.InstallLocation -as [string]).Trim('"')
if ((Normalize-PathForCompare $installLocation) -ine (Normalize-PathForCompare $good)) {
    throw "InstallLocation was '$installLocation', expected '$good'."
}
if (-not $reg.QuietUninstallString) {
    throw 'QuietUninstallString registry value is missing.'
}

$staleInternalLocation = Join-Path $env:TEMP 'Brick Stale Internal Location'
& reg.exe add 'HKCU\Software\Advance\Brick' /ve /t REG_SZ /d $staleInternalLocation /f | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'Failed to simulate a stale internal install-location registry value.'
}

$second = Start-Process -FilePath $installer.FullName -ArgumentList "/S /D=$badSecond" -Wait -PassThru
if ($second.ExitCode -ne 0) {
    throw "Second silent installer run failed with exit code $($second.ExitCode)."
}
if (Test-Path (Join-Path $badSecond 'brick.exe')) {
    throw 'Brick incorrectly installed into the second alternate /D path.'
}
if (-not (Test-Path (Join-Path $good 'brick.exe'))) {
    throw 'Brick was missing from LOCALAPPDATA after the second installer run.'
}

$manufacturerKey = Get-Item -Path 'HKCU:\Software\Advance\Brick'
$manufacturerLocation = ($manufacturerKey.GetValue('') -as [string]).Trim('"')
if ((Normalize-PathForCompare $manufacturerLocation) -ine (Normalize-PathForCompare $good)) {
    throw "Internal install location was '$manufacturerLocation', expected '$good'."
}

Start-Process -FilePath (Join-Path $good 'uninstall.exe') -ArgumentList '/S' -Wait
