$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $testRoot | Out-Null
$oldProgramFiles = ${env:ProgramFiles(x86)}
$oldThumbprint = $env:BRICK_CERTUM_THUMBPRINT
$thumbprint = 'A' * 40
$global:BrickSigningTestSignature = $null
$global:BrickSigningTestExitCode = 0
$global:BrickSigningTestCalls = [Collections.Generic.List[object]]::new()
$stub = Join-Path $testRoot 'signtool.ps1'
@'
$global:BrickSigningTestCalls.Add(@($args))
$global:LASTEXITCODE = $global:BrickSigningTestExitCode
'@ | Set-Content -LiteralPath $stub

# Model the OS trust result separately from SignTool's exit status. The release
# candidate exercises both real implementations against the cloud certificate.
function Get-ChildItem {
    param([string]$Path)
    [pscustomobject]@{
        FullName = $stub
        Directory = [pscustomobject]@{ Parent = [pscustomobject]@{ Name = '10.0.1.0' } }
    }
}
function Get-AuthenticodeSignature {
    param([string]$LiteralPath)
    return $global:BrickSigningTestSignature
}
function Assert-Rejected {
    param([scriptblock]$Run, [string]$Expected)
    $message = $null
    try { & $Run | Out-Null } catch { $message = $_.Exception.Message }
    if (-not $message -or $message -notlike "*$Expected*") {
        throw "Expected rejection '$Expected', got '$message'."
    }
    Write-Output "Rejected: $Expected"
}

try {
    ${env:ProgramFiles(x86)} = $testRoot
    $env:BRICK_CERTUM_THUMBPRINT = $thumbprint
    $file = Join-Path $testRoot 'brick.exe'
    $second = Join-Path $testRoot 'brick-setup.exe'
    'fixture' | Set-Content $file
    'fixture' | Set-Content $second
    $sign = Join-Path $PSScriptRoot 'sign-windows.ps1'

    Assert-Rejected { & $sign -Path $file -Thumbprint 'invalid' -VerifyOnly } '40 hexadecimal'
    Assert-Rejected { & $sign -Path $file -Thumbprint '' } 'required for signing'
    Assert-Rejected { & $sign -Path (Join-Path $testRoot '*.missing') -VerifyOnly } 'No Windows executables matched'
    Assert-Rejected { & $sign -Path $stub -VerifyOnly } 'Only Windows .exe'

    $global:BrickSigningTestSignature = [pscustomobject]@{
        Status = 'NotSigned'
        SignerCertificate = [pscustomobject]@{ Thumbprint = $thumbprint }
        TimeStamperCertificate = [pscustomobject]@{ Subject = 'Timestamp authority' }
    }
    Assert-Rejected { & $sign -Path $file -VerifyOnly } 'Invalid Authenticode signature'
    $global:BrickSigningTestSignature.Status = 'HashMismatch'
    Assert-Rejected { & $sign -Path $file -VerifyOnly } 'HashMismatch'
    $global:BrickSigningTestSignature.Status = 'Valid'
    $global:BrickSigningTestSignature.SignerCertificate.Thumbprint = 'B' * 40
    Assert-Rejected { & $sign -Path $file -VerifyOnly } 'Unexpected signing certificate'
    $global:BrickSigningTestSignature.SignerCertificate.Thumbprint = $thumbprint
    $global:BrickSigningTestSignature.TimeStamperCertificate = $null
    Assert-Rejected { & $sign -Path $file -VerifyOnly } 'Missing timestamp'
    $global:BrickSigningTestSignature.TimeStamperCertificate = [pscustomobject]@{ Subject = 'Timestamp authority' }
    $global:BrickSigningTestExitCode = 1
    Assert-Rejected { & $sign -Path $file -VerifyOnly } 'SignTool verification failed'

    $global:BrickSigningTestExitCode = 0
    $global:BrickSigningTestCalls.Clear()
    & $sign -Path $file, $second -VerifyOnly
    if ($global:BrickSigningTestCalls.Count -ne 2) { throw 'Both executables must be verified.' }
    foreach ($call in $global:BrickSigningTestCalls) {
        foreach ($flag in @('verify', '/pa', '/all', '/tw')) {
            if ($flag -notin $call) { throw "SignTool verification is missing $flag." }
        }
    }
    Write-Output 'Windows signing verification tests passed.'
} finally {
    ${env:ProgramFiles(x86)} = $oldProgramFiles
    $env:BRICK_CERTUM_THUMBPRINT = $oldThumbprint
    Remove-Variable BrickSigningTestExitCode, BrickSigningTestCalls, BrickSigningTestSignature -Scope Global
    Remove-Item -LiteralPath $testRoot -Recurse -Force
}
