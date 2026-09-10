param(
    [Parameter(Mandatory)]
    [string]$Path
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# NSIS !uninstfinalize supplies a .tmp filename. Sign an adjacent .exe copy,
# then return the verified bytes to NSIS without relaxing the normal signer.
$file = Get-Item -LiteralPath $Path
if ($file.PSIsContainer) { throw 'NSIS uninstaller must be a file.' }
$stream = $file.OpenRead()
try {
    if ($stream.ReadByte() -ne 0x4d -or $stream.ReadByte() -ne 0x5a) {
        throw 'NSIS uninstaller is not a Windows executable.'
    }
} finally {
    $stream.Dispose()
}

$signable = $file.FullName + '.' + [guid]::NewGuid().ToString('N') + '.exe'
try {
    Copy-Item -LiteralPath $file.FullName -Destination $signable
    & (Join-Path $PSScriptRoot 'sign-windows.ps1') -Path $signable
    Copy-Item -LiteralPath $signable -Destination $file.FullName -Force
} finally {
    Remove-Item -LiteralPath $signable -ErrorAction SilentlyContinue
}
