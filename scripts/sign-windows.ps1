param(
    [Parameter(Mandatory)]
    [string[]]$Path,
    [string]$Thumbprint = $env:BRICK_CERTUM_THUMBPRINT,
    [switch]$VerifyOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Thumbprint = ($Thumbprint -replace '\s', '').ToUpperInvariant()
if ($Thumbprint -and $Thumbprint -notmatch '^[0-9A-F]{40}$') {
    throw 'The signing certificate thumbprint must contain 40 hexadecimal characters.'
}
if (-not $VerifyOnly -and -not $Thumbprint) {
    throw 'BRICK_CERTUM_THUMBPRINT is required for signing.'
}

$files = @(Resolve-Path -Path $Path | ForEach-Object { Get-Item -LiteralPath $_.Path })
if ($files.Count -eq 0) { throw 'No Windows executables matched the signing paths.' }
foreach ($file in $files) {
    if ($file.PSIsContainer -or $file.Extension -ine '.exe') {
        throw 'Only Windows .exe files may be passed to this signing script.'
    }
}

$sdkRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits/10/bin'
$signTool = Get-ChildItem -Path "$sdkRoot/*/x64/signtool.exe" |
    Sort-Object { [version]$_.Directory.Parent.Name } -Descending |
    Select-Object -First 1
if (-not $signTool) { throw 'The Windows SDK x64 SignTool was not found.' }

if (-not $VerifyOnly) {
    $certificate = Get-Item -LiteralPath "Cert:\CurrentUser\My\$Thumbprint"
    if (-not $certificate.HasPrivateKey) {
        throw 'The signing certificate has no accessible private key. Check the SimplySign connection.'
    }
    $now = Get-Date
    if ($now -lt $certificate.NotBefore -or $now -ge $certificate.NotAfter) {
        throw 'The signing certificate is not currently valid.'
    }
    $usage = @($certificate.Extensions | Where-Object { $_.Oid.Value -eq '2.5.29.37' })
    if ($usage.Count -ne 1 -or '1.3.6.1.5.5.7.3.3' -notin @($usage[0].EnhancedKeyUsages.Value)) {
        throw 'The selected certificate does not allow code signing.'
    }
}

foreach ($file in $files) {
    if (-not $VerifyOnly) {
        & $signTool.FullName sign /sha1 $Thumbprint /fd SHA256 /tr http://time.certum.pl /td SHA256 /v $file.FullName
        if ($LASTEXITCODE -ne 0) { throw "Signing failed for $($file.Name)." }
    }

    $signature = Get-AuthenticodeSignature -LiteralPath $file.FullName
    if ($signature.Status -ne 'Valid') {
        throw "Invalid Authenticode signature on $($file.Name): $($signature.Status)."
    }
    if ($Thumbprint -and $signature.SignerCertificate.Thumbprint -ne $Thumbprint) {
        throw "Unexpected signing certificate on $($file.Name)."
    }
    if (-not $signature.TimeStamperCertificate) {
        throw "Missing timestamp on $($file.Name)."
    }
    & $signTool.FullName verify /pa /all /v /tw $file.FullName
    if ($LASTEXITCODE -ne 0) { throw "SignTool verification failed for $($file.Name)." }
    Write-Output "Verified signature and timestamp: $($file.Name)"
}
