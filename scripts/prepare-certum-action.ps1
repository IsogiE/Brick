param([string]$ActionScriptPath)
$ErrorActionPreference = 'Stop'

# Reviewed adaptation of the pinned upstream action. Fail closed if upstream
# bytes differ; authentication, TOTP generation and certificate checks stay intact.
$revision = '89ae3b032d4bc7a5b98d1a42a34e61ecb6faad64'
if (-not $ActionScriptPath) {
    if (-not $env:RUNNER_WORKSPACE) { throw 'The Actions workspace is required.' }
    $ActionScriptPath = Join-Path (Split-Path $env:RUNNER_WORKSPACE -Parent) "_actions/dismine/windows-app-signing-setup-action/$revision/Connect-SimplySign-Enhanced.ps1"
}
if ((Get-FileHash -LiteralPath $ActionScriptPath -Algorithm SHA256).Hash -ne '3e248c7161aa29c609a78c8afa351d091aa2742cd73c90fc216028664d742616') {
    throw 'The pinned SimplySign authentication script has unexpected bytes.'
}
$source = [IO.File]::ReadAllText($ActionScriptPath).Replace("`r`n", "`n")
function Replace-Once([string]$Before, [string]$After) {
    if (($script:source.Split(@($Before), [StringSplitOptions]::None)).Count -ne 2) {
        throw 'The reviewed SimplySign patch no longer matches exactly once.'
    }
    $script:source = $script:source.Replace($Before, $After)
}

$windowChecks = @'
public class WinAPI {
    [DllImport("user32.dll")]
    public static extern bool IsIconic(IntPtr hWnd);
    [DllImport("user32.dll")]
    public static extern bool EnumChildWindows(IntPtr hWnd, EnumWindowsProc callback, IntPtr parameter);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetClassName(IntPtr hWnd, StringBuilder name, int count);

    // Identify the login form by its two native input controls. Never read
    // their contents or infer readiness from a transient window's dimensions.
    public static int CredentialFieldCount(IntPtr window) {
        int count = 0;
        EnumChildWindows(window, (child, parameter) => {
            var name = new StringBuilder(256);
            GetClassName(child, name, name.Capacity);
            if (IsWindowVisible(child) && name.ToString().StartsWith("WindowsForms10.EDIT.", StringComparison.Ordinal)) count++;
            return true;
        }, IntPtr.Zero);
        return count;
    }
'@
Replace-Once 'public class WinAPI {' $windowChecks
Replace-Once 'if (wPid == pid && IsWindowVisible(hWnd)) {' 'if (wPid == pid && IsWindowVisible(hWnd) && !IsIconic(hWnd)) {'
Replace-Once 'foreach ($h in $Windows) {' @'
foreach ($h in $Windows) {
        if ([WinAPI]::CredentialFieldCount($h) -ne 2) { continue }
'@
Replace-Once 'if ($windows.Count -gt 0) {' 'if ((Get-LoginWindow -Windows $windows) -ne [IntPtr]::Zero) {'
Replace-Once 'if ($windows.Count -eq 0) {' 'if ((Get-LoginWindow -Windows $windows) -eq [IntPtr]::Zero) {'
Replace-Once 'SimplySign Desktop did not open any windows within $maxWaitSeconds seconds' 'SimplySign Desktop did not expose its two-field login form within $maxWaitSeconds seconds'

[IO.File]::WriteAllText($ActionScriptPath, $source, [Text.UTF8Encoding]::new($false))
Write-Host 'Verified pinned SimplySign action and applied the reviewed login readiness fix.'
