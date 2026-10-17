<#
.SYNOPSIS
    Sample any process's CPU and memory into a CSV, using the same counters
    RSClip's built-in metrics use.

.DESCRIPTION
    Exists so RSClip and a competing recorder (OBS's replay buffer) can be
    measured by one method. A comparison where each side is measured
    differently proves nothing.

    CPU is reported as a percentage of ONE logical core, matching
    ProcReading::cpu_pct_core in src/metrics/proc.rs. Divide by the core count
    for percent-of-machine.

.PARAMETER Name
    Process name without .exe (e.g. obs64, rsclip).

.PARAMETER Seconds
    Total sampling duration. Default 600 (10 minutes).

.PARAMETER Interval
    Seconds between samples. Default 10, matching METRICS_INTERVAL_SECS.

.PARAMETER Out
    Output CSV path. Defaults to .\<name>-<timestamp>.csv

.EXAMPLE
    .\sample-process.ps1 -Name obs64 -Seconds 600
    .\sample-process.ps1 -Name rsclip -Seconds 600
#>
param(
    [Parameter(Mandatory = $true)][string]$Name,
    [int]$Seconds = 600,
    [int]$Interval = 10,
    [string]$Out
)

$ErrorActionPreference = 'Stop'

if (-not $Out) {
    $Out = ".\{0}-{1}.csv" -f $Name, (Get-Date -Format 'yyyyMMdd_HHmmss')
}

$proc = Get-Process -Name $Name -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $proc) {
    Write-Error "No running process named '$Name'. Start it first."
}

$cores = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors
Write-Host "Sampling '$Name' (PID $($proc.Id)) for ${Seconds}s every ${Interval}s -> $Out"
Write-Host "Logical cores: $cores"

"t_s,cpu_pct_core,cpu_pct_machine,working_set_mb,private_mb" | Out-File -FilePath $Out -Encoding utf8

$prevCpu = $proc.TotalProcessorTime.TotalSeconds
$prevAt  = Get-Date
$started = Get-Date

while (((Get-Date) - $started).TotalSeconds -lt $Seconds) {
    Start-Sleep -Seconds $Interval

    # Refresh cached counters; a stale handle reports the values from startup.
    $proc.Refresh()
    if ($proc.HasExited) {
        Write-Warning "Process exited early; stopping."
        break
    }

    $now      = Get-Date
    $cpu      = $proc.TotalProcessorTime.TotalSeconds
    $wallSec  = ($now - $prevAt).TotalSeconds
    $cpuPct   = if ($wallSec -gt 0) { (($cpu - $prevCpu) / $wallSec) * 100 } else { 0 }

    $row = "{0:F1},{1:F3},{2:F3},{3:F2},{4:F2}" -f `
        ($now - $started).TotalSeconds,
        $cpuPct,
        ($cpuPct / $cores),
        ($proc.WorkingSet64 / 1MB),
        ($proc.PrivateMemorySize64 / 1MB)

    $row | Out-File -FilePath $Out -Append -Encoding utf8
    Write-Host ("  {0}s  cpu {1:F2}% of a core  ws {2:F1} MB" -f `
        [int]($now - $started).TotalSeconds, $cpuPct, ($proc.WorkingSet64 / 1MB))

    $prevCpu = $cpu
    $prevAt  = $now
}

# Median is the headline figure: a single scheduling hiccup should not decide
# what goes on a CV.
$rows = Import-Csv $Out
if ($rows.Count -gt 0) {
    $cpuVals = $rows | ForEach-Object { [double]$_.cpu_pct_core } | Sort-Object
    $wsVals  = $rows | ForEach-Object { [double]$_.working_set_mb } | Sort-Object
    $mid     = [int]([math]::Floor($cpuVals.Count / 2))
    Write-Host ""
    Write-Host "Samples        : $($rows.Count)"
    Write-Host ("CPU median     : {0:F2}% of one core ({1:F3}% of machine)" -f $cpuVals[$mid], ($cpuVals[$mid] / $cores))
    Write-Host ("CPU max        : {0:F2}% of one core" -f $cpuVals[-1])
    Write-Host ("Working set med: {0:F1} MB" -f $wsVals[$mid])
    Write-Host ("Working set max: {0:F1} MB" -f $wsVals[-1])
}
Write-Host "CSV: $Out"
