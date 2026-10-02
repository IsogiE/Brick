# UTC is checked in the disposable signing runner, never in the installed app.
function Get-BrickSigningClockAdjustment {
    param([string]$Date, [string]$Age, [double]$ElapsedSeconds, [DateTimeOffset]$LocalTime)
    if (-not $Date -or ($Age -and $Age -ne '0')) { throw 'Signing time response is missing or cached.' }
    if ([double]::IsNaN($ElapsedSeconds) -or $ElapsedSeconds -lt 0 -or $ElapsedSeconds -gt 4) {
        throw 'Signing time request exceeded the uncertainty limit.'
    }
    $remote = [DateTimeOffset]::MinValue
    if (-not [DateTimeOffset]::TryParseExact($Date, 'r', [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::AssumeUniversal, [ref]$remote)) {
        throw 'Signing time response has an invalid UTC date.'
    }
    # HTTP dates have one-second resolution; account for that and half the RTT.
    $adjustment = $remote.AddSeconds(0.5 + $ElapsedSeconds / 2) - $LocalTime
    if ([math]::Abs($adjustment.TotalSeconds) -gt 300) { throw 'Signing runner clock exceeds the correction limit.' }
    return $adjustment
}

function Sync-BrickSigningClock {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $request = @{
        Uri = 'https://api.github.com/meta?brick-signing-time=' + [Guid]::NewGuid().ToString('N')
        Headers = @{'Cache-Control'='no-cache'; 'Accept'='application/vnd.github+json'}
        UserAgent = 'Brick-signing-clock'
        MaximumRedirection = 0
        TimeoutSec = 10
        UseBasicParsing = $true
        ErrorAction = 'Stop'
    }
    $response = Invoke-WebRequest @request
    $timer.Stop()
    $arguments = @{
        Date = [string](@($response.Headers['Date'])[0])
        Age = [string](@($response.Headers['Age'])[0])
        ElapsedSeconds = $timer.Elapsed.TotalSeconds
        LocalTime = [DateTimeOffset]::UtcNow
    }
    $adjustment = Get-BrickSigningClockAdjustment @arguments
    Set-Date -Adjust $adjustment -ErrorAction Stop | Out-Null
    Write-Host ('Checked signing runner UTC; correction {0:F2} seconds, request {1:F2} seconds.' -f $adjustment.TotalSeconds, $timer.Elapsed.TotalSeconds)
}

function Get-BrickSigningUnixTime { return [DateTimeOffset]::UtcNow.ToUnixTimeSeconds() }

function Wait-BrickSigningTotpWindow {
    param([int]$Period, [long]$AfterPeriod = -1)
    if ($Period -lt 3 -or $Period -gt 300) { throw 'Unsupported signing token period.' }
    $minimumRemaining = [math]::Min(20, $Period - 2)
    for ($attempt = 0; $attempt -lt 6; $attempt++) {
        # Recheck after sleeping: a drifting VM can wake before the real boundary.
        Sync-BrickSigningClock
        $seconds = Get-BrickSigningUnixTime
        $secondsLeft = $Period - ($seconds % $Period)
        if ([math]::Floor($seconds / $Period) -gt $AfterPeriod -and $secondsLeft -ge $minimumRemaining) { return }
        Start-Sleep -Seconds ($secondsLeft + 1)
    }
    throw 'Could not obtain a fresh verified signing token window.'
}
