$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
$directory = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory $directory | Out-Null
$forms = [Collections.Generic.List[Windows.Forms.Form]]::new()
try {
    $path = Join-Path $directory 'Connect-SimplySign-Enhanced.ps1'
    Invoke-WebRequest 'https://raw.githubusercontent.com/dismine/windows-app-signing-setup-action/89ae3b032d4bc7a5b98d1a42a34e61ecb6faad64/Connect-SimplySign-Enhanced.ps1' -OutFile $path
    & "$PSScriptRoot/prepare-certum-action.ps1" -ActionScriptPath $path
    $source = [IO.File]::ReadAllText($path)
    $match = [regex]::Match($source, '(?s)Add-Type @"\n(using System;\nusing System.Runtime.InteropServices;.*?)\n"@')
    if (-not $match.Success) { throw 'Could not locate the verified window API.' }
    Add-Type -TypeDefinition $match.Groups[1].Value
    $tokens = $null; $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw 'Patched action is not valid PowerShell.' }
    $function = $ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Get-LoginWindow' }, $true)
    if ($function.Count -ne 1) { throw 'Login selector is ambiguous.' }
    . ([scriptblock]::Create($function[0].Extent.Text))
    function New-Window([int]$Width, [int]$Height) {
        $form = [Windows.Forms.Form]::new()
        $form.Text = 'SimplySign Desktop'
        $form.Size = [Drawing.Size]::new($Width, $Height)
        $forms.Add($form)
        return $form
    }
    function Select-Login { return Get-LoginWindow -Windows ([WinAPI]::GetVisibleWindows([uint32]$PID)) }
    $startup = New-Window 800 600
    $startup.WindowState = [Windows.Forms.FormWindowState]::Minimized
    $startup.Show()
    $popup = New-Window 183 159; $popup.Show()
    $large = New-Window 1100 700; $large.Show()
    [Windows.Forms.Application]::DoEvents()
    if ((Select-Login) -ne [IntPtr]::Zero) { throw 'Startup or popup window was mistaken for a login form.' }
    if ([WinAPI]::GetVisibleWindows([uint32]$PID).Contains($startup.Handle)) { throw 'Minimized startup window was retained as a popup.' }
    $login = New-Window 479 408
    $userField = [Windows.Forms.TextBox]::new(); $userField.Text = 'fixture-user'
    $tokenField = [Windows.Forms.TextBox]::new(); $tokenField.Top = 45; $tokenField.Text = 'fixture-token'
    $login.Controls.Add($userField); $login.Controls.Add($tokenField)
    $login.Show(); [Windows.Forms.Application]::DoEvents()
    if ((Select-Login) -ne $login.Handle) { throw 'Delayed login form was not selected.' }
    $login.Enabled = $false
    if ((Select-Login) -ne $login.Handle) { throw 'A modal popup prevented identifying its login owner.' }
    if ($userField.Text -ne 'fixture-user' -or $tokenField.Text -ne 'fixture-token') { throw 'Window discovery changed credential fields.' }
    $login.Controls.Remove($tokenField)
    if ((Select-Login) -ne [IntPtr]::Zero) { throw 'An incomplete form was accepted.' }
    $rejected = $false
    try { & "$PSScriptRoot/prepare-certum-action.ps1" -ActionScriptPath $path } catch { $rejected = $_.Exception.Message -like '*unexpected bytes*' }
    if (-not $rejected) { throw 'Modified upstream bytes were accepted.' }
    Write-Host 'Passed real-window regressions for delayed login, minimized startup, popups, incomplete forms and modified upstream bytes; no credentials used.'
} finally {
    foreach ($form in $forms) { $form.Dispose() }
    Remove-Item -LiteralPath $directory -Recurse -Force
}
