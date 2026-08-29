param(
  [int]$Port = 7373,
  [Parameter(Mandatory=$true)]
  [string]$LanCidr,
  [Parameter(Mandatory=$true)]
  [string]$TailscaleCidr,
  [string]$RuleName = "distributed-watchdog HTTP"
)

$ErrorActionPreference = "Stop"
$remoteAddresses = @($LanCidr, $TailscaleCidr)

$rule = Get-NetFirewallRule -DisplayName $RuleName -ErrorAction SilentlyContinue
if ($null -eq $rule) {
  New-NetFirewallRule `
    -DisplayName $RuleName `
    -Direction Inbound `
    -Action Allow `
    -Protocol TCP `
    -LocalPort $Port `
    -RemoteAddress $remoteAddresses `
    -Profile Private,Domain | Out-Null
} else {
  $rule | Set-NetFirewallRule -Enabled True -Action Allow -Profile Private,Domain
  $rule | Get-NetFirewallPortFilter | Set-NetFirewallPortFilter -Protocol TCP -LocalPort $Port
  $rule | Get-NetFirewallAddressFilter | Set-NetFirewallAddressFilter -RemoteAddress $remoteAddresses
}

Write-Host "allowed TCP $Port from $($remoteAddresses -join ', ')"
