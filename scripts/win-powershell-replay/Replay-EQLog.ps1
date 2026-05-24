<#
.SYNOPSIS
    Replay a saved EverQuest log file into a watched file at real-time (or scaled) pace.

.DESCRIPTION
    Reads lines from a source EQ log file and appends them to a target file at
    intervals derived from the EQ timestamps embedded in each line.  Point the
    froklog tray client at the target file (named eqlog_<Player>_<Server>.txt)
    and it will process the traffic exactly as if the game were running live.

.PARAMETER Source
    Path to the source EverQuest log file.

.PARAMETER Target
    Path to the output file the tray client is watching.
    Must be named eqlog_<Player>_<Server>.txt so the client can extract the
    player name.  The file is created (or truncated) before replay starts.

.PARAMETER Speed
    Replay speed multiplier.  1.0 = real-time, 10.0 = 10x faster.
    Ignored when -Dump is set.  Default: 1.0.

.PARAMETER From
    Start replaying from this timestamp (format: "YYYY-MM-DD HH:MM:SS").
    Lines before this timestamp are skipped.  Omit to start from the beginning.

.PARAMETER To
    Stop replaying at this timestamp (format: "YYYY-MM-DD HH:MM:SS").
    Omit to replay until EOF.

.PARAMETER Dump
    Send all lines as fast as possible and exit.  Overrides -Speed.

.EXAMPLE
    # Real-time replay of the last hour of a log
    .\Replay-EQLog.ps1 `
        -Source "C:\EQ\Logs\eqlog_Icestorm_Teek.txt" `
        -Target "C:\EQ\Logs\eqlog_Icestorm_Test.txt" `
        -From "2026-05-17 19:00:00"

.EXAMPLE
    # 20x speed replay of a specific combat window
    .\Replay-EQLog.ps1 `
        -Source "C:\EQ\Logs\eqlog_Icestorm_Teek.txt" `
        -Target "C:\EQ\Logs\eqlog_Icestorm_Test.txt" `
        -From "2026-05-17 20:15:00" `
        -To   "2026-05-17 20:45:00" `
        -Speed 20.0

.EXAMPLE
    # Dump mode — blast through the whole file as fast as possible
    .\Replay-EQLog.ps1 `
        -Source "C:\EQ\Logs\eqlog_Icestorm_Teek.txt" `
        -Target "C:\EQ\Logs\eqlog_Icestorm_Test.txt" `
        -Dump
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Source,
    [Parameter(Mandatory)][string]$Target,
    [double]$Speed = 1.0,
    [string]$From,
    [string]$To,
    [switch]$Dump
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── Timestamp parsing ─────────────────────────────────────────────────────────

# Matches [Tue Jan  1 20:00:01 2026] or [Tue Jan 01 20:00:01 2026]
$tsRegex = [regex]'^\\[\\w{3} \\w{3} +\\d{1,2} \\d{2}:\\d{2}:\\d{2} \\d{4}\\]'

# .NET format strings — try space-padded day first, then zero-padded
$tsFormats = @('ddd MMM  d HH:mm:ss yyyy', 'ddd MMM dd HH:mm:ss yyyy')
$culture   = [System.Globalization.CultureInfo]::InvariantCulture

function Parse-EQTimestamp([string]$line) {
    if ($line.Length -lt 26 -or $line[0] -ne '[') { return $null }
    $raw = $line.Substring(1, 24)
    foreach ($fmt in $tsFormats) {
        $dt = [datetime]::MinValue
        if ([datetime]::TryParseExact($raw, $fmt, $culture, 'None', [ref]$dt)) {
            return $dt
        }
    }
    return $null
}

# ── Argument validation ───────────────────────────────────────────────────────

if (-not (Test-Path $Source)) {
    Write-Error "Source file not found: $Source"
    exit 1
}

$fromDt = $null
$toDt   = $null
if ($From) { $fromDt = [datetime]::Parse($From) }
if ($To)   { $toDt   = [datetime]::Parse($To) }

if ($Speed -le 0) {
    Write-Error "-Speed must be greater than zero."
    exit 1
}

# ── Open files ────────────────────────────────────────────────────────────────

# Truncate (or create) the target so the tray client starts from a clean EOF.
$writer = [System.IO.StreamWriter]::new($Target, $false, [System.Text.Encoding]::UTF8)
$writer.AutoFlush = $true   # each Add flushes so the tailer sees it within 50 ms

$reader = [System.IO.StreamReader]::new($Source, [System.Text.Encoding]::UTF8)

# ── Replay loop ───────────────────────────────────────────────────────────────

$skipping  = ($null -ne $fromDt)
$firstTs   = $null
$startWall = $null
$lineCount = 0
$sentCount = 0

Write-Host "Source : $Source"
Write-Host "Target : $Target"
if ($fromDt) { Write-Host "From   : $fromDt" }
if ($toDt)   { Write-Host "To     : $toDt" }
if ($Dump)   { Write-Host "Mode   : dump (no pacing)" }
else         { Write-Host "Speed  : ${Speed}x" }
Write-Host ""

try {
    while (-not $reader.EndOfStream) {
        $line = $reader.ReadLine()
        $lineCount++

        $ts = Parse-EQTimestamp $line

        # Skip lines before --From
        if ($skipping) {
            if ($null -ne $ts -and $ts -ge $fromDt) { $skipping = $false }
            else { continue }
        }

        # Stop at --To
        if ($null -ne $toDt -and $null -ne $ts -and $ts -gt $toDt) {
            Write-Host "Reached -To cutoff, stopping."
            break
        }

        # Pace output to match log timestamps (skipped in dump mode)
        if (-not $Dump -and $null -ne $ts) {
            if ($null -eq $firstTs) {
                $firstTs   = $ts
                $startWall = [datetime]::UtcNow
            } else {
                $logMs      = ($ts - $firstTs).TotalMilliseconds / $Speed
                $wallTarget = $startWall.AddMilliseconds($logMs)
                $sleepMs    = ($wallTarget - [datetime]::UtcNow).TotalMilliseconds
                if ($sleepMs -gt 10) {
                    Start-Sleep -Milliseconds ([int]$sleepMs)
                }
            }
        }

        $writer.WriteLine($line)
        $sentCount++
    }
} finally {
    $writer.Close()
    $reader.Close()
}

Write-Host "Done. Sent $sentCount / $lineCount lines."
