$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/sync-signing-clock.ps1"
$date = 'Fri, 02 Oct 2026 20:00:00 GMT'
$local = [DateTimeOffset]::Parse('2026-10-02T20:01:01Z', [Globalization.CultureInfo]::InvariantCulture)
$adjustment = Get-BrickSigningClockAdjustment -Date $date -ElapsedSeconds 1 -LocalTime $local
if ($adjustment.TotalSeconds -ne -60) { throw 'Ahead-clock correction is incorrect.' }
$adjustment = Get-BrickSigningClockAdjustment -Date $date -Age '0' -ElapsedSeconds 0 -LocalTime $local.AddSeconds(-120)
if ($adjustment.TotalSeconds -ne 59.5) { throw 'Behind-clock correction is incorrect.' }
$invalid = @(
    @{Date=''; ElapsedSeconds=0; LocalTime=$local},
    @{Date='bad date'; ElapsedSeconds=0; LocalTime=$local},
    @{Date=$date; Age='60'; ElapsedSeconds=0; LocalTime=$local},
    @{Date=$date; Age='bad'; ElapsedSeconds=0; LocalTime=$local},
    @{Date=$date; ElapsedSeconds=4.01; LocalTime=$local},
    @{Date=$date; ElapsedSeconds=-1; LocalTime=$local},
    @{Date=$date; ElapsedSeconds=[double]::NaN; LocalTime=$local},
    @{Date=$date; ElapsedSeconds=0; LocalTime=$local.AddHours(1)}
)
foreach ($arguments in $invalid) {
    $rejected = $false
    try { $null = Get-BrickSigningClockAdjustment @arguments } catch { $rejected = $true }
    if (-not $rejected) { throw 'Unsafe signing time was accepted.' }
}
# Simulate early wakeups without networking, credentials, sleeping or changing
# the test machine's clock. Each retry must obtain fresh verified time.
function Sync-BrickSigningClock { $script:syncCount++ }
function Get-BrickSigningUnixTime {
    $value = $script:times[$script:index]
    $script:index++
    return $value
}
function Start-Sleep { param([int]$Seconds) $script:waits += $Seconds }
$script:syncCount = 0; $script:index = 0; $script:waits = @(); $script:times = @(1001,1002,1022)
Wait-BrickSigningTotpWindow -Period 30
if ($script:syncCount -ne 3 -or ($script:waits -join ',') -ne '20,19') { throw 'Early wakeup was not rechecked.' }
$script:syncCount = 0; $script:index = 0; $script:waits = @(); $script:times = @(1021,1051)
Wait-BrickSigningTotpWindow -Period 30 -AfterPeriod 34
if ($script:syncCount -ne 2 -or ($script:waits -join ',') -ne '30') { throw 'Recovery reused the rejected period.' }
$script:index = 0; $script:times = @(1001,1001,1001,1001,1001,1001)
$rejected = $false
try { Wait-BrickSigningTotpWindow -Period 30 } catch { $rejected = $true }
if (-not $rejected -or $script:index -ne 6) { throw 'Clock-window retries were not bounded.' }
function Sync-BrickSigningClock { throw 'Time source unavailable.' }
$rejected = $false
try { Wait-BrickSigningTotpWindow -Period 30 } catch { $rejected = $true }
if (-not $rejected) { throw 'An unverified clock was used for signing.' }
Write-Host 'Passed clock correction, stale/invalid/uncertain time rejection, early wakeup, fresh recovery period and unavailable time-source checks; no credentials used.'
