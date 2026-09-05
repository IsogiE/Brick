param(
    [Parameter(Mandatory=$true)][string]$Installer,
    [string]$OutputDirectory = "$env:USERPROFILE\Desktop\Brick-smoke",
    [int]$SampleSeconds = 60,
    [string]$SoftwareOpenGLDirectory
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutputDirectory | Out-Null
Start-Transcript -Path (Join-Path $OutputDirectory 'transcript.txt') -Force | Out-Null
$report = [ordered]@{ started = (Get-Date).ToString('o'); passed = $false; checks = @() }
try {
    if ([System.Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0) { throw 'Run this harness in the logged-in desktop session, not service session 0.' }
    $principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run the smoke test without elevation to verify current-user installation.' }
    if (Get-Process brick -ErrorAction SilentlyContinue) { throw 'Close the existing Brick test instance first.' }
    Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class BrickWindow {
    public delegate bool EnumProc(IntPtr hwnd, IntPtr lParam);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc proc, IntPtr lParam);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint msg, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")] static extern int GetWindowTextLength(IntPtr hwnd);
    public static IntPtr Find(int targetPid) {
        IntPtr result = IntPtr.Zero;
        EnumWindows((hwnd, unused) => { uint pid; GetWindowThreadProcessId(hwnd, out pid);
            if (pid == targetPid && GetWindowTextLength(hwnd) > 0) { result = hwnd; return false; } return true;
        }, IntPtr.Zero);
        return result;
    }
}
'@
    function Assert-Check([bool]$Condition, [string]$Name) {
        $report.checks += [ordered]@{ name = $Name; passed = $Condition; timestamp = (Get-Date).ToString('o') }
        if (!$Condition) { throw "FAIL: $Name" }
        Write-Output "PASS: $Name"
    }
    function Measure-Brick([System.Diagnostics.Process]$Process, [string]$Phase, [IntPtr]$Window) {
        $rows = @()
        $logical = [Environment]::ProcessorCount
        $timer = [Diagnostics.Stopwatch]::StartNew()
        $Process.Refresh()
        $previousCpu = $Process.TotalProcessorTime.TotalSeconds
        $previousTime = $timer.Elapsed.TotalSeconds
        for ($i=0; $i -lt $SampleSeconds; $i++) {
            Start-Sleep -Seconds 1
            $Process.Refresh()
            if ($Process.HasExited) { throw "Brick exited during $Phase" }
            $now = $timer.Elapsed.TotalSeconds
            $cpu = $Process.TotalProcessorTime.TotalSeconds
            $corePercent = 100 * ($cpu-$previousCpu) / ($now-$previousTime)
            $rows += [pscustomobject]@{
                phase = $Phase; elapsedSeconds = $now; cpuOneCorePercent = $corePercent
                cpuTaskManagerPercent = $corePercent/$logical; workingSetMB = $Process.WorkingSet64/1MB
                visible = [BrickWindow]::IsWindowVisible($Window); minimized = [BrickWindow]::IsIconic($Window)
                foreground = ([BrickWindow]::GetForegroundWindow() -eq $Window)
            }
            $previousCpu = $cpu; $previousTime = $now
        }
        $rows | Export-Csv -NoTypeInformation (Join-Path $OutputDirectory "$Phase.csv")
        $report[$Phase] = [ordered]@{
            seconds = $timer.Elapsed.TotalSeconds; logicalProcessors = $logical
            averageTaskManagerCpuPercent = ($rows | Measure-Object cpuTaskManagerPercent -Average).Average
            maxTaskManagerCpuPercent = ($rows | Measure-Object cpuTaskManagerPercent -Maximum).Maximum
            averageWorkingSetMB = ($rows | Measure-Object workingSetMB -Average).Average
        }
        if ($Phase -eq 'hidden') {
            Assert-Check (@($rows | Where-Object { $_.visible -and !$_.minimized }).Count -eq 0) 'Background interval never showed the hidden window'
        }
    }
    $report.installerSha256 = (Get-FileHash $Installer -Algorithm SHA256).Hash
    $report.os = (Get-CimInstance Win32_OperatingSystem).Caption
    $report.uac = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System').EnableLUA
    $report.graphics = @(Get-CimInstance Win32_VideoController | Select-Object Name,DriverVersion)
    # Install a local copy so UNC-zone prompts do not interrupt automated runs.
    $localInstaller = Join-Path $env:TEMP ('Brick-smoke-' + $report.installerSha256 + '.exe')
    Copy-Item $Installer $localInstaller -Force
    $install = Start-Process $localInstaller -ArgumentList '/S' -Wait -PassThru
    Assert-Check ($install.ExitCode -eq 0) 'Installer succeeds without elevation'
    $exe = Join-Path $env:LOCALAPPDATA 'Brick\brick.exe'
    Assert-Check (Test-Path $exe) 'Installed under LOCALAPPDATA\Brick'
    $report.binarySha256 = (Get-FileHash $exe -Algorithm SHA256).Hash
    $report.softwareOpenGL = [bool]$SoftwareOpenGLDirectory
    if ($SoftwareOpenGLDirectory) {
        # VM fixture only; these files are never included in the released app.
        Copy-Item (Join-Path $SoftwareOpenGLDirectory '*.dll') (Split-Path $exe) -Force
        $env:GALLIUM_DRIVER = 'llvmpipe'
        $env:LP_NUM_THREADS = [string][Math]::Min(4, [Environment]::ProcessorCount)
        $report.openGLDlls = @(Get-ChildItem (Join-Path $SoftwareOpenGLDirectory '*.dll') | Get-FileHash -Algorithm SHA256 | Select-Object Hash,Path)
    }
    $brick = Start-Process $exe -PassThru
    Start-Sleep -Seconds 10
    $brick.Refresh()
    Assert-Check (!$brick.HasExited) 'Brick remains running after launch'
    $window = [BrickWindow]::Find($brick.Id)
    Assert-Check ($window -ne [IntPtr]::Zero -and [BrickWindow]::IsWindowVisible($window)) 'Main window is visible'
    [BrickWindow]::PostMessage($window, 0x0112, [IntPtr]0xF020, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Seconds 2
    Assert-Check (![BrickWindow]::IsWindowVisible($window)) 'Minimize button hides Brick into the tray'
    $second = Start-Process $exe -PassThru
    $second.WaitForExit(5000) | Out-Null
    Start-Sleep -Seconds 2
    Assert-Check ([BrickWindow]::IsWindowVisible($window) -and ![BrickWindow]::IsIconic($window)) 'Restores after using the minimize button'
    Measure-Brick $brick 'visible' $window
    [BrickWindow]::PostMessage($window, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Seconds 2
    $brick.Refresh()
    Assert-Check (!$brick.HasExited -and ![BrickWindow]::IsWindowVisible($window)) 'X hides the window and keeps Brick running'
    Measure-Brick $brick 'hidden' $window
    $second = Start-Process $exe -PassThru
    $second.WaitForExit(5000) | Out-Null
    Start-Sleep -Seconds 2
    Assert-Check ([BrickWindow]::IsWindowVisible($window) -and ![BrickWindow]::IsIconic($window)) 'Second launch restores the existing window'
    Assert-Check (@(Get-Process brick).Count -eq 1) 'Only one Brick process remains'
    $brick.Kill(); $brick.WaitForExit()
    $brick = Start-Process $exe -ArgumentList '--startup' -PassThru
    Start-Sleep -Seconds 10
    $brick.Refresh()
    $window = [BrickWindow]::Find($brick.Id)
    Assert-Check (!$brick.HasExited -and $window -ne [IntPtr]::Zero -and ![BrickWindow]::IsWindowVisible($window)) 'Start minimized keeps the window hidden'
    $report.passed = $true
} catch {
    $report.error = $_.ToString()
    Write-Output $_
} finally {
    $report.finished = (Get-Date).ToString('o')
    $report | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $OutputDirectory 'report.json')
    Stop-Transcript | Out-Null
}
if (!$report.passed) { exit 1 }
