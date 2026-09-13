$ErrorActionPreference = 'Stop'
$directory = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory $directory | Out-Null
$fixture = $null
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
    function Set-Stage([string]$Stage) {
        [IO.File]::WriteAllText((Join-Path $directory 'command.txt'), $Stage)
        $deadline = [DateTime]::UtcNow.AddSeconds(15)
        while ([DateTime]::UtcNow -lt $deadline) {
            $fixture.Refresh()
            if ($fixture.HasExited) { throw 'Fixture exited before the readiness assertion.' }
            try {
                $state = [IO.File]::ReadAllText((Join-Path $directory 'state.json')) | ConvertFrom-Json
                if ($state.stage -eq $Stage) { return $state }
            } catch [IO.IOException] { } catch [ArgumentException] { }
            Start-Sleep -Milliseconds 100
        }
        throw "Fixture did not reach stage $Stage."
    }
    [IO.File]::WriteAllText((Join-Path $directory 'command.txt'), 'initial')
    $fixturePath = Join-Path $PSScriptRoot 'certum-window-fixture.ps1'
    $fixture = Start-Process "$env:SystemRoot/System32/WindowsPowerShell/v1.0/powershell.exe" -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',"`"$fixturePath`"",'-Directory',"`"$directory`"") -PassThru -WindowStyle Hidden
    function Select-Login { return Get-LoginWindow -Windows ([WinAPI]::GetVisibleWindows([uint32]$fixture.Id)) }
    $state = Set-Stage 'initial'
    if ((Select-Login) -ne [IntPtr]::Zero) { throw 'Startup or popup window was mistaken for a login form.' }
    if ([WinAPI]::GetVisibleWindows([uint32]$fixture.Id).Contains([IntPtr]$state.startupHandle)) { throw 'Minimized startup window was retained as a popup.' }
    $state = Set-Stage 'ready'
    if ((Select-Login) -ne [IntPtr]$state.loginHandle) { throw 'Delayed login form was not selected.' }
    $state = Set-Stage 'disabled'
    if ((Select-Login) -ne [IntPtr]$state.loginHandle) { throw 'A modal popup prevented identifying its login owner.' }
    if (-not $state.valuesPreserved) { throw 'Window discovery changed credential fields.' }
    $null = Set-Stage 'incomplete'
    if ((Select-Login) -ne [IntPtr]::Zero) { throw 'An incomplete form was accepted.' }
    $rejected = $false
    try { & "$PSScriptRoot/prepare-certum-action.ps1" -ActionScriptPath $path } catch { $rejected = $_.Exception.Message -like '*unexpected bytes*' }
    if (-not $rejected) { throw 'Modified upstream bytes were accepted.' }
    Write-Host 'Passed separate-process Windows regressions for delayed login, minimized startup, popups, incomplete forms and modified upstream bytes; no credentials used.'
} finally {
    if ($fixture) {
        [IO.File]::WriteAllText((Join-Path $directory 'command.txt'), 'stop')
        if (-not $fixture.WaitForExit(3000)) { $fixture.Kill(); $fixture.WaitForExit() }
        $fixture.Dispose()
    }
    Remove-Item -LiteralPath $directory -Recurse -Force
}
