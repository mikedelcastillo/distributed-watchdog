param(
  [string]$InstallDir = (Get-Location).Path,
  [string]$ExePath = "",
  [string]$ConfigPath = "",
  [string]$StdoutPath = "",
  [string]$StderrPath = ""
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($ExePath)) {
  $ExePath = Join-Path $InstallDir "distributed-watchdog.exe"
}
if ([string]::IsNullOrWhiteSpace($ConfigPath)) {
  $ConfigPath = Join-Path $InstallDir "config.toml"
}
if ([string]::IsNullOrWhiteSpace($StdoutPath)) {
  $StdoutPath = Join-Path $InstallDir "logs\distributed-watchdog.out.log"
}
if ([string]::IsNullOrWhiteSpace($StderrPath)) {
  $StderrPath = Join-Path $InstallDir "logs\distributed-watchdog.err.log"
}

if (!(Test-Path -LiteralPath $ExePath)) {
  throw "missing executable $ExePath"
}
if (!(Test-Path -LiteralPath $ConfigPath)) {
  throw "missing config $ConfigPath"
}
foreach ($LogPath in @($StdoutPath, $StderrPath)) {
  $Parent = Split-Path -Parent $LogPath
  if (![string]::IsNullOrWhiteSpace($Parent)) {
    New-Item -ItemType Directory -Force -Path $Parent | Out-Null
  }
}

$ExpectedExe = (Resolve-Path -LiteralPath $ExePath).Path
Get-Process distributed-watchdog -ErrorAction SilentlyContinue |
  Where-Object {
    try { $_.Path -eq $ExpectedExe } catch { $false }
  } |
  Stop-Process -Force

$process = Start-Process `
  -FilePath $ExePath `
  -ArgumentList @("--config", $ConfigPath, "serve") `
  -WorkingDirectory $InstallDir `
  -WindowStyle Hidden `
  -RedirectStandardOutput $StdoutPath `
  -RedirectStandardError $StderrPath `
  -PassThru

Start-Sleep -Seconds 2
if ($process.HasExited) {
  $stderr = if (Test-Path -LiteralPath $StderrPath) { Get-Content -LiteralPath $StderrPath -Tail 20 } else { "" }
  throw "distributed-watchdog exited during startup. $stderr"
}

Write-Host "started distributed-watchdog pid $($process.Id)"
