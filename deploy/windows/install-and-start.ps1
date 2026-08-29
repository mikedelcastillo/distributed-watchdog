param(
  [string]$InstallDir = (Get-Location).Path,
  [string]$TaskName = "distributed-watchdog",
  [switch]$RunElevated,
  [switch]$RunAsSystem
)

$ErrorActionPreference = "Stop"
$InstallDir = (Resolve-Path -LiteralPath $InstallDir).Path
$exePath = Join-Path $InstallDir "distributed-watchdog.exe"
$configPath = Join-Path $InstallDir "config.toml"
$envPath = Join-Path $InstallDir ".env"
$installer = Join-Path $InstallDir "install-scheduled-task.ps1"

foreach ($staleName in @("$TaskName;", "-ConfigPath", "-ExePath", "$TaskName")) {
  $task = Get-ScheduledTask -TaskName $staleName -ErrorAction SilentlyContinue
  if ($null -ne $task -and $staleName -ne $TaskName) {
    Unregister-ScheduledTask -TaskName $staleName -Confirm:$false
  }
}

if ($RunAsSystem) {
  & $installer `
    -InstallDir $InstallDir `
    -ExePath $exePath `
    -ConfigPath $configPath `
    -EnvPath $envPath `
    -TaskName $TaskName `
    -RunAsSystem
} else {
  if ($RunElevated) {
    & $installer `
      -InstallDir $InstallDir `
      -ExePath $exePath `
      -ConfigPath $configPath `
      -EnvPath $envPath `
      -TaskName $TaskName `
      -RunElevated
  } else {
    & $installer `
      -InstallDir $InstallDir `
      -ExePath $exePath `
      -ConfigPath $configPath `
      -EnvPath $envPath `
      -TaskName $TaskName
  }
}
Start-ScheduledTask -TaskName $TaskName
Start-Sleep -Seconds 3

$info = Get-ScheduledTaskInfo -TaskName $TaskName
Write-Host "task-state: $((Get-ScheduledTask -TaskName $TaskName).State)"
Write-Host "last-result: $($info.LastTaskResult)"

$process = Get-Process distributed-watchdog -ErrorAction SilentlyContinue
if ($null -eq $process) {
  Write-Host "process: not-running"
} else {
  $process | Select-Object Id, ProcessName | Format-Table -AutoSize
}
