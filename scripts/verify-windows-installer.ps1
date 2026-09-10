param(
    [switch]$RunInstallSmoke,
    [switch]$RequireSignature,
    [string]$ExpectedSigner
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
if ($content -notmatch 'INSTALLMODE "both"' -or $content -notmatch 'SelectInstallDir') {
    throw 'The installer must support both current-user and administrator installation scopes.'
}
if ($content -notmatch 'Page custom InstallScopePage InstallScopeLeave' -or $content -notmatch 'ExecShellWait "runas"') {
    throw 'The installer must offer installation scope and request elevation for all users.'
}
if ($content -notmatch '(?s)!include MultiUser.nsh.*?RequestExecutionLevel user') {
    throw 'The installer must remain launchable by the existing 0.4.5 updater.'
}
if ($content -notmatch 'QuietUninstallString') {
    throw 'The installer did not register QuietUninstallString.'
}
if ($content -notmatch 'ALLOWDOWNGRADES "false"') {
    throw 'The installer was rendered with downgrades allowed.'
}

# CI images have Visual C++ runtimes installed and can mask missing DLLs.
# Check the built PE imports before relying on the installer smoke test.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$visualStudio = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$dumpbin = Get-ChildItem (Join-Path $visualStudio 'VC/Tools/MSVC/*/bin/Hostx64/x64/dumpbin.exe') |
    Sort-Object FullName -Descending | Select-Object -First 1
if (-not $dumpbin) { throw 'Visual Studio dumpbin was not found.' }
$dependencies = & $dumpbin.FullName /DEPENDENTS (Join-Path $repoRoot 'target/release/brick.exe')
if ($LASTEXITCODE -ne 0) { throw 'Failed to inspect Brick PE imports.' }
if ($dependencies -match '(?i)(VCRUNTIME|MSVCP|CONCRT)[0-9_]*D?\.dll') {
    throw 'Brick still depends on an external Visual C++ runtime. Build with -C target-feature=+crt-static.'
}
Write-Output 'Brick has no external Visual C++ runtime DLL dependency.'

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

$first = Start-Process -FilePath $installer.FullName -ArgumentList "/S /CurrentUser /D=$bad" -Wait -PassThru
if ($first.ExitCode -ne 0) {
    throw "First silent installer run failed with exit code $($first.ExitCode)."
}
if (-not (Test-Path (Join-Path $good 'brick.exe'))) {
    throw 'Brick was not installed under LOCALAPPDATA.'
}
if ($RequireSignature) {
    & (Join-Path $PSScriptRoot 'sign-windows.ps1') -Path (Join-Path $good 'brick.exe') -Thumbprint $ExpectedSigner -VerifyOnly
    if ($env:BRICK_WINDOWS_SIGNING -eq 'certum') {
        & (Join-Path $PSScriptRoot 'sign-windows.ps1') -Path (Join-Path $good 'uninstall.exe') -Thumbprint $ExpectedSigner -VerifyOnly
    }
    $builtHash = (Get-FileHash (Join-Path $repoRoot 'target/release/brick.exe') -Algorithm SHA256).Hash
    if ((Get-FileHash (Join-Path $good 'brick.exe') -Algorithm SHA256).Hash -ne $builtHash) {
        throw 'The installer did not preserve the signed Brick executable.'
    }
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

$second = Start-Process -FilePath $installer.FullName -ArgumentList "/S /CurrentUser /D=$badSecond" -Wait -PassThru
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

# Hosted Windows CI runs elevated. Check that an all-users install uses the
# machine registry and protected Program Files directory, including a rerun.
$machine = Join-Path $env:ProgramFiles 'Brick'
$duplicate = Start-Process -FilePath $installer.FullName -ArgumentList '/S /AllUsers /NS' -Wait -PassThru
if ($duplicate.ExitCode -ne 2 -or (Test-Path (Join-Path $machine 'brick.exe'))) {
    throw 'The installer allowed a new machine copy alongside the current-user installation.'
}
Start-Process -FilePath (Join-Path $good 'uninstall.exe') -ArgumentList '/S /CurrentUser' -Wait
if (Test-Path (Join-Path $good 'brick.exe')) { throw 'Current-user uninstall failed.' }
foreach ($attempt in 1..2) {
    $installed = Start-Process -FilePath $installer.FullName -ArgumentList '/S /AllUsers /NS' -Wait -PassThru
    if ($installed.ExitCode -ne 0) { throw "Machine install failed with exit code $($installed.ExitCode)." }
    if (-not (Test-Path (Join-Path $machine 'brick.exe'))) { throw 'Administrator install did not use Program Files.' }
    $registration = Get-ItemProperty 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Brick'
    if ((Normalize-PathForCompare $registration.InstallLocation.Trim('"')) -ine (Normalize-PathForCompare $machine)) {
        throw 'Machine registration points to the wrong installation directory.'
    }
    if ($RequireSignature) {
        & (Join-Path $PSScriptRoot 'sign-windows.ps1') -Path (Join-Path $machine 'brick.exe') -Thumbprint $ExpectedSigner -VerifyOnly
        if ((Get-FileHash (Join-Path $machine 'brick.exe') -Algorithm SHA256).Hash -ne $builtHash) {
            throw 'The machine installer did not preserve the signed Brick executable.'
        }
    }
}
$duplicate = Start-Process -FilePath $installer.FullName -ArgumentList '/S /CurrentUser /NS' -Wait -PassThru
if ($duplicate.ExitCode -ne 2 -or (Test-Path (Join-Path $good 'brick.exe'))) {
    throw 'The installer allowed a new current-user copy alongside the machine installation.'
}

# Recreate a legacy current-user copy to test updates when a computer already
# has both scopes. New installers reject creating this state themselves.
Copy-Item -Path $machine -Destination $good -Recurse
New-Item $regPath -Force | Out-Null
New-ItemProperty $regPath -Name InstallLocation -Value $good -PropertyType String -Force | Out-Null
New-ItemProperty $regPath -Name UninstallString -Value "`"$good\uninstall.exe`"" -PropertyType String -Force | Out-Null
# An elevated 0.4.5 client supplies /D= but no scope. With both installations
# present it must still replace the current-user copy. Use its license as a
# harmless sentinel so installing to the wrong scope cannot pass unnoticed.
Set-Content (Join-Path $good 'LICENSE') 'legacy update fixture'
$legacy = Start-Process -FilePath $installer.FullName -ArgumentList "/S /NS /D=$good" -Wait -PassThru
if ($legacy.ExitCode -ne 0) { throw 'The legacy update invocation failed.' }
if ((Get-FileHash (Join-Path $good 'LICENSE')).Hash -ne (Get-FileHash (Join-Path $repoRoot 'LICENSE')).Hash) {
    throw 'An elevated legacy update did not update the current-user installation.'
}
Start-Process -FilePath (Join-Path $good 'uninstall.exe') -ArgumentList '/S /CurrentUser' -Wait
if (-not (Test-Path (Join-Path $machine 'brick.exe'))) { throw 'Current-user uninstall affected the machine installation.' }
Start-Process -FilePath (Join-Path $machine 'uninstall.exe') -ArgumentList '/S /AllUsers' -Wait
