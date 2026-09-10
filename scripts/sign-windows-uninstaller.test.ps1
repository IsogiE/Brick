$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ("Brick NSIS O'Brien " + [guid]::NewGuid())
New-Item -ItemType Directory -Path $testRoot | Out-Null
try {
    $helper = Join-Path $testRoot 'sign-windows-uninstaller.ps1'
    Copy-Item (Join-Path $PSScriptRoot 'sign-windows-uninstaller.ps1') $helper
    $signer = Join-Path $testRoot 'sign-windows.ps1'
    @'
param([string]$Path)
if ([IO.Path]::GetExtension($Path) -ne '.exe') { throw 'Signer requires .exe.' }
[IO.File]::AppendAllText($Path, 'verified signature fixture')
'@ | Set-Content $signer

    foreach ($name in @('nst123.tmp', 'makensis123')) {
        $inputFile = Join-Path $testRoot $name
        [IO.File]::WriteAllText($inputFile, 'MZ unsigned fixture')
        & $helper -Path $inputFile
        if ([IO.File]::ReadAllText($inputFile) -ne 'MZ unsigned fixtureverified signature fixture') {
            throw 'Signed bytes were not returned to the NSIS temporary file.'
        }
        if (@(Get-ChildItem $testRoot -Filter '*.exe').Count) { throw 'Signing copy was not cleaned up.' }
    }

    'throw "Signing fixture rejected"' | Set-Content $signer
    $inputFile = Join-Path $testRoot 'failure.tmp'
    [IO.File]::WriteAllText($inputFile, 'MZ original fixture')
    $message = ''
    try { & $helper -Path $inputFile } catch { $message = $_.Exception.Message }
    if ($message -ne 'Signing fixture rejected') { throw 'Signing failure was not propagated.' }
    if ([IO.File]::ReadAllText($inputFile) -ne 'MZ original fixture') { throw 'Signing failure changed the input.' }
    if (@(Get-ChildItem $testRoot -Filter '*.exe').Count) { throw 'Failed signing left its temporary copy.' }

    [IO.File]::WriteAllText($inputFile, 'not an executable')
    $message = ''
    try { & $helper -Path $inputFile } catch { $message = $_.Exception.Message }
    if ($message -ne 'NSIS uninstaller is not a Windows executable.') { throw 'Non-executable input was accepted.' }
    Write-Output 'NSIS uninstaller signing handoff tests passed.'
} finally {
    Remove-Item -LiteralPath $testRoot -Recurse -Force
}
