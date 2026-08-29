param(
  [string]$InstallDir = "$env:ProgramData\distributed-watchdog",
  [string]$ExePath = "",
  [string]$ConfigPath = "",
  [string]$EnvPath = "",
  [string]$TaskName = "distributed-watchdog",
  [switch]$RunElevated,
  [switch]$RunAsSystem
)

$ErrorActionPreference = "Stop"

$currentIdentity = [Security.Principal.WindowsIdentity]::GetCurrent()
$currentPrincipal = New-Object Security.Principal.WindowsPrincipal($currentIdentity)
if (!$currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
  throw "this installer must be run from an elevated PowerShell Administrator session"
}

if ([string]::IsNullOrWhiteSpace($TaskName) -or $TaskName -match '[\\/:*?"<>|]') {
  throw "TaskName must be a non-empty task name without path or filename separators"
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$InstallDir = (Resolve-Path -LiteralPath $InstallDir).Path

if ([string]::IsNullOrWhiteSpace($ExePath)) {
  $ExePath = Join-Path $InstallDir "distributed-watchdog.exe"
}
if ([string]::IsNullOrWhiteSpace($ConfigPath)) {
  $ConfigPath = Join-Path $InstallDir "config.toml"
}
if ([string]::IsNullOrWhiteSpace($EnvPath)) {
  $EnvPath = Join-Path $InstallDir ".env"
}

if (!(Test-Path -LiteralPath $ExePath -PathType Leaf)) {
  throw "missing executable $ExePath"
}
if (!(Test-Path -LiteralPath $ConfigPath -PathType Leaf)) {
  throw "missing config $ConfigPath"
}

$ExePath = (Resolve-Path -LiteralPath $ExePath).Path
$ConfigPath = (Resolve-Path -LiteralPath $ConfigPath).Path
if (Test-Path -LiteralPath $EnvPath -PathType Leaf) {
  $EnvPath = (Resolve-Path -LiteralPath $EnvPath).Path
} else {
  $EnvPath = [System.IO.Path]::GetFullPath($EnvPath)
}
$installPrefix = $InstallDir.TrimEnd('\') + '\'
foreach ($Path in @($ExePath, $ConfigPath, $EnvPath)) {
  if (!$Path.StartsWith($installPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "executable, config, and .env paths must be within $InstallDir"
  }
}

if (!(Test-Path -LiteralPath $EnvPath -PathType Leaf)) {
  New-Item -ItemType File -Path $EnvPath -Force | Out-Null
}

$adminSid = New-Object System.Security.Principal.SecurityIdentifier("S-1-5-32-544")
$systemSid = New-Object System.Security.Principal.SecurityIdentifier("S-1-5-18")
$userSid = $currentIdentity.User
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

# Some existing files may have inheritance disabled already, so grant their
# effective permissions directly instead of relying only on directory ACEs.
$ProtectedFiles = @($ExePath, $ConfigPath, $EnvPath)
$ProtectedFiles += @(
  Get-ChildItem -LiteralPath $InstallDir -Filter "*.ps1" -File -ErrorAction SilentlyContinue |
    Select-Object -ExpandProperty FullName
)
foreach ($ProtectedFile in @($ProtectedFiles | Select-Object -Unique)) {
  & icacls $ProtectedFile `
    /inheritance:r `
    /grant:r "*S-1-5-18:F" "*S-1-5-32-544:F" "*$($userSid.Value):RX" | Out-Null
  if ($LASTEXITCODE -ne 0) {
    throw "failed to harden ACLs on $ProtectedFile"
  }
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

$launchCommand = "& '" + $ExePath.Replace("'", "''") + "' --config '" + $ConfigPath.Replace("'", "''") + "' serve"
$encodedLaunchCommand = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($launchCommand))
$powerShellPath = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
$action = New-ScheduledTaskAction `
  -Execute $powerShellPath `
  -Argument "-NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand $encodedLaunchCommand" `
  -WorkingDirectory $InstallDir

if ($RunAsSystem) {
  $trigger = New-ScheduledTaskTrigger -AtStartup
  $principal = New-ScheduledTaskPrincipal `
    -UserId "SYSTEM" `
    -LogonType ServiceAccount `
    -RunLevel Highest
  $mode = "SYSTEM startup at highest privilege. No interactive credentials are stored."
  $screenshotNote = "Screenshots are usually unavailable because SYSTEM has no interactive desktop."
} else {
  $taskUser = $currentIdentity.Name
  if ([string]::IsNullOrWhiteSpace($taskUser) -or $taskUser -eq "NT AUTHORITY\SYSTEM") {
    throw "the default task requires an interactive Administrator account; use -RunAsSystem for a headless install"
  }
  $trigger = New-ScheduledTaskTrigger -AtLogOn -User $taskUser
  $principal = New-ScheduledTaskPrincipal `
    -UserId $taskUser `
    -LogonType Interactive `
    -RunLevel Highest
  $mode = "interactive current-user logon at highest privilege. No password is stored."
  $screenshotNote = "Screenshots can access this user's desktop while the session is active."
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

& icacls $InstallDir /setowner "*S-1-5-32-544" /T /C | Out-Null
if ($LASTEXITCODE -ne 0) {
  throw "failed to assign protected install ownership"
}

Write-Host "Installed scheduled task $TaskName"
Write-Host "Config: $ConfigPath"
Write-Host "Env: $EnvPath"
if ($RunElevated -or $RunAsSystem) {
  if ($RunElevated -and !$RunAsSystem) {
    Write-Warning "-RunElevated is retained for compatibility; the default task already runs at highest privilege."
  }
}
Write-Host "Mode: $mode"
Write-Host $screenshotNote
