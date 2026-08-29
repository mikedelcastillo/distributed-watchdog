param(
  [string]$InstallDir = "$env:ProgramData\distributed-watchdog",
  [string]$ExePath = "$env:ProgramData\distributed-watchdog\distributed-watchdog.exe",
  [string]$ConfigPath = "$env:ProgramData\distributed-watchdog\config.toml",
  [string]$EnvPath = "$env:ProgramData\distributed-watchdog\.env",
  [string]$TaskName = "distributed-watchdog",
  [switch]$RunElevated,
  [switch]$RunAsSystem
)

$ErrorActionPreference = "Stop"

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

if (!(Test-Path -LiteralPath $ExePath)) {
  throw "missing executable $ExePath"
}
if (!(Test-Path -LiteralPath $ConfigPath)) {
  throw "missing config $ConfigPath"
}

$adminSid = New-Object System.Security.Principal.SecurityIdentifier("S-1-5-32-544")
$systemSid = New-Object System.Security.Principal.SecurityIdentifier("S-1-5-18")
$userSid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
$acl = Get-Acl -LiteralPath $InstallDir
$acl.SetAccessRuleProtection($true, $false)
foreach ($rule in @($acl.Access)) {
  [void]$acl.RemoveAccessRule($rule)
}
foreach ($entry in @(
  @($adminSid, "FullControl"),
  @($systemSid, "FullControl"),
  @($userSid, "ReadAndExecute")
)) {
  $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
    $entry[0],
    $entry[1],
    "ContainerInherit,ObjectInherit",
    "None",
    "Allow"
  )
  $acl.AddAccessRule($rule)
}
Set-Acl -LiteralPath $InstallDir -AclObject $acl
& icacls $InstallDir `
  /inheritance:r `
  /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*$($userSid.Value):(OI)(CI)RX" `
  /T | Out-Null
if ($LASTEXITCODE -ne 0) {
  throw "failed to harden ACLs on $InstallDir"
}

foreach ($WritableDir in @(
  (Join-Path $InstallDir ".watchdog-state"),
  (Join-Path $InstallDir "logs")
)) {
  New-Item -ItemType Directory -Force -Path $WritableDir | Out-Null
  & icacls $WritableDir `
    /inheritance:r `
    /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*$($userSid.Value):(OI)(CI)M" `
    /T | Out-Null
  if ($LASTEXITCODE -ne 0) {
    throw "failed to set writable ACLs on $WritableDir"
  }
}

$action = New-ScheduledTaskAction `
  -Execute $ExePath `
  -Argument "--config `"$ConfigPath`" serve" `
  -WorkingDirectory $InstallDir

if ($RunAsSystem) {
  $trigger = New-ScheduledTaskTrigger -AtStartup
  $principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -RunLevel Highest
} else {
  $taskUser = "$env:COMPUTERNAME\$env:USERNAME"
  $trigger = New-ScheduledTaskTrigger -AtLogOn -User $taskUser
  $runLevel = if ($RunElevated) { "Highest" } else { "Limited" }
  $principal = New-ScheduledTaskPrincipal `
    -UserId $taskUser `
    -LogonType Interactive `
    -RunLevel $runLevel
}

$settings = New-ScheduledTaskSettingsSet `
  -AllowStartIfOnBatteries `
  -ExecutionTimeLimit (New-TimeSpan -Days 0) `
  -RestartCount 999 `
  -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask `
  -TaskName $TaskName `
  -Action $action `
  -Trigger $trigger `
  -Principal $principal `
  -Settings $settings `
  -Force | Out-Null

Write-Host "Installed scheduled task $TaskName"
Write-Host "Config: $ConfigPath"
Write-Host "Env: $EnvPath"
if ($RunAsSystem) {
  Write-Host "Mode: SYSTEM startup. Screenshots usually will not capture an interactive desktop."
} else {
  if ($RunElevated) {
    Write-Host "Mode: elevated interactive user logon. Use only when shutdown support requires it."
  } else {
    Write-Host "Mode: interactive user logon. This is recommended for screenshot support."
  }
}
