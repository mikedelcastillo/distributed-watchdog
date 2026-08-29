param(
  [string]$TaskName = "distributed-watchdog",
  [string]$InstallDir = (Get-Location).Path,
  [string]$ExePath = ""
)

$ErrorActionPreference = "SilentlyContinue"

if ([string]::IsNullOrWhiteSpace($ExePath)) {
  $ExePath = Join-Path $InstallDir "distributed-watchdog.exe"
}

Stop-ScheduledTask -TaskName $TaskName
if (Test-Path -LiteralPath $ExePath) {
  $ExpectedExe = (Resolve-Path -LiteralPath $ExePath).Path
  Get-Process distributed-watchdog -ErrorAction SilentlyContinue |
    Where-Object {
      try { $_.Path -eq $ExpectedExe } catch { $false }
    } |
    Stop-Process -Force
}
Start-Sleep -Seconds 1

if (Test-Path -LiteralPath $ExePath) {
  $ExpectedExe = (Resolve-Path -LiteralPath $ExePath).Path
  $StillRunning = Get-Process distributed-watchdog -ErrorAction SilentlyContinue |
    Where-Object {
      try { $_.Path -eq $ExpectedExe } catch { $false }
    }
}

if ($StillRunning) {
  Write-Host "process: still-running"
  exit 1
}

Write-Host "process: stopped"
