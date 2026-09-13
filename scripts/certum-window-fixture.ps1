param([Parameter(Mandatory)][string]$Directory)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
# Match the pinned SimplySign application's .NET Framework UI and message loop.
function New-Window([int]$Width, [int]$Height) {
    $form = [Windows.Forms.Form]::new()
    $form.Text = 'SimplySign Desktop'
    $form.Size = [Drawing.Size]::new($Width, $Height)
    return $form
}
$startup = New-Window 800 600
$startup.WindowState = [Windows.Forms.FormWindowState]::Minimized
$popup = New-Window 183 159; $popup.Show()
$large = New-Window 1100 700; $large.Show()
$login = New-Window 479 408
$userField = [Windows.Forms.TextBox]::new(); $userField.Text = 'fixture-user'
$tokenField = [Windows.Forms.TextBox]::new(); $tokenField.Top = 45; $tokenField.Text = 'fixture-token'
$login.Controls.Add($userField); $login.Controls.Add($tokenField)
$script:stage = ''
$timer = [Windows.Forms.Timer]::new(); $timer.Interval = 100
$timer.Add_Tick({
    $command = [IO.File]::ReadAllText((Join-Path $Directory 'command.txt')).Trim()
    if ($command -eq $script:stage) { return }
    switch ($command) {
        'initial' { }
        'ready' { $login.Show() }
        'disabled' { $login.Enabled = $false }
        'incomplete' { $login.Controls.Remove($tokenField) }
        'stop' { $startup.Close(); return }
        default { throw 'Unexpected fixture command.' }
    }
    $script:stage = $command
    $state = @{ stage=$command; pid=$PID; startupHandle=$startup.Handle.ToInt64(); loginHandle=$login.Handle.ToInt64(); valuesPreserved=($userField.Text -eq 'fixture-user' -and $tokenField.Text -eq 'fixture-token') }
    [IO.File]::WriteAllText((Join-Path $Directory 'state.json'), ($state | ConvertTo-Json))
})
try {
    $timer.Start()
    [Windows.Forms.Application]::Run($startup)
} finally {
    $timer.Dispose()
    foreach ($form in @($login,$large,$popup,$startup)) { $form.Dispose() }
}
