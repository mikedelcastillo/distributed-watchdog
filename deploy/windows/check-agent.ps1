param(
  [string]$InstallDir = (Get-Location).Path,
  [int]$Port = 7373
)

$ErrorActionPreference = "Stop"
Set-Location -LiteralPath $InstallDir

Write-Host "config:"
& (Join-Path $InstallDir "distributed-watchdog.exe") --config (Join-Path $InstallDir "config.toml") config-check

Write-Host "process:"
$process = Get-Process distributed-watchdog -ErrorAction SilentlyContinue
if ($null -eq $process) {
  Write-Host "not-running"
} else {
  $process | Select-Object Id, ProcessName | Format-Table -AutoSize
}

Write-Host "listener:"
$listener = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
if ($null -eq $listener) {
  Write-Host "not-listening"
} else {
  $listener | Select-Object LocalAddress, LocalPort | Format-Table -AutoSize
}

Write-Host "local-health:"
$envPath = Join-Path $InstallDir ".env"
$secret = $null
if (Test-Path -LiteralPath $envPath) {
  foreach ($line in Get-Content -LiteralPath $envPath) {
    if ($line -match '^\s*CLUSTER_SECRET=(.*)$') {
      $secret = $Matches[1]
      break
    }
  }
}
if ([string]::IsNullOrWhiteSpace($secret)) {
  Write-Host "missing-secret"
} else {
  try {
    $health = Invoke-RestMethod `
      -Uri "http://127.0.0.1:$Port/health" `
      -Headers @{ "x-watchdog-secret" = $secret } `
      -TimeoutSec 5
    Write-Host "ok node=$($health.node_id) leader=$($health.leader_id)"
  } catch {
    Write-Host "failed"
  }
}

Write-Host "firewall:"
$rule = Get-NetFirewallRule -DisplayName "distributed-watchdog HTTP" -ErrorAction SilentlyContinue
if ($null -eq $rule) {
  Write-Host "missing-rule"
} else {
  $rule | Select-Object DisplayName, Enabled, Direction, Action | Format-Table -AutoSize
  $rule | Get-NetFirewallAddressFilter | Select-Object RemoteAddress | Format-Table -AutoSize
}

Write-Host "stderr-tail:"
$stderrPath = Join-Path $InstallDir "distributed-watchdog.err.log"
if (Test-Path -LiteralPath $stderrPath) {
  Get-Content -LiteralPath $stderrPath -Tail 20
} else {
  Write-Host "no-stderr-log"
}
