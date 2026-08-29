param(
  [Parameter(Mandatory=$true)]
  [string]$RepoUrl,
  [string]$Branch = "main",
  [string]$InstallDir = (Get-Location).Path,
  [string]$SourceDir = "",
  [string]$TaskName = "distributed-watchdog",
  [string]$BinaryUrl = "",
  [string]$ChecksumUrl = "",
  [string]$AssetName = "distributed-watchdog-windows-x86_64.exe",
  [int]$DelaySeconds = 2,
  [switch]$Detached,
  [switch]$LockAlreadyHeld
)

$ErrorActionPreference = "Stop"
$InstallDir = (Resolve-Path -LiteralPath $InstallDir).Path
if (Test-Path -LiteralPath (Join-Path $InstallDir ".git") -PathType Container) {
  throw "InstallDir must be a dedicated runtime directory, not a Git checkout"
}
if ([string]::IsNullOrWhiteSpace($SourceDir)) {
  $SourceDir = Join-Path $InstallDir "source"
}
$SourceParent = Split-Path -Parent $SourceDir
if (!(Test-Path -LiteralPath $SourceParent)) {
  New-Item -ItemType Directory -Force -Path $SourceParent | Out-Null
}
$ResolvedParent = (Resolve-Path -LiteralPath $SourceParent).Path
$InstallCompare = $InstallDir.TrimEnd("\")
$ParentCompare = $ResolvedParent.TrimEnd("\")
if (
  $ParentCompare -ne $InstallCompare -and
  !$ParentCompare.StartsWith("$InstallCompare\", [System.StringComparison]::OrdinalIgnoreCase)
) {
  throw "SourceDir must be inside InstallDir"
}
$LogPath = Join-Path $InstallDir "update.log"
$LockPath = Join-Path $InstallDir "update.lock"
$StaleLockSeconds = 7200
$PowerShellPath = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
if (!(Test-Path -LiteralPath $PowerShellPath)) {
  $PowerShellPath = "powershell.exe"
}

function Try-AcquireUpdateLock {
  if (Test-Path -LiteralPath $LockPath) {
    $lockAge = (New-TimeSpan -Start (Get-Item -LiteralPath $LockPath).LastWriteTimeUtc -End ([DateTime]::UtcNow)).TotalSeconds
    $pidText = (Get-Content -LiteralPath $LockPath -ErrorAction SilentlyContinue | Select-Object -First 1)
    $pidValue = 0
    if ([int]::TryParse($pidText, [ref]$pidValue)) {
      $existing = Get-Process -Id $pidValue -ErrorAction SilentlyContinue
      if ($null -ne $existing) {
        return $false
      }
    }
    if ($lockAge -lt $StaleLockSeconds) {
      return $false
    }
    Remove-Item -LiteralPath $LockPath -Force
  }

  try {
    New-Item -ItemType File -Path $LockPath -Value "$PID`n" -ErrorAction Stop | Out-Null
    return $true
  } catch {
    return $false
  }
}

if (!$Detached) {
  if (!(Try-AcquireUpdateLock)) {
    Write-Host "update already running"
    exit 0
  }
  $args = @(
    "-NoProfile",
    "-ExecutionPolicy", "Bypass",
    "-File", $PSCommandPath,
    "-RepoUrl", $RepoUrl,
    "-Branch", $Branch,
    "-InstallDir", $InstallDir,
    "-SourceDir", $SourceDir,
    "-TaskName", $TaskName,
    "-DelaySeconds", $DelaySeconds,
    "-Detached",
    "-LockAlreadyHeld"
  )
  if (![string]::IsNullOrWhiteSpace($BinaryUrl)) {
    $args += @("-BinaryUrl", $BinaryUrl, "-ChecksumUrl", $ChecksumUrl, "-AssetName", $AssetName)
  }
  Start-Process -FilePath $PowerShellPath -ArgumentList $args -WindowStyle Hidden
  Write-Host "update scheduled"
  exit 0
}

if ($LockAlreadyHeld) {
  Set-Content -LiteralPath $LockPath -Value "$PID"
} elseif (!(Try-AcquireUpdateLock)) {
  Write-Host "update already running"
  exit 0
}

$TranscriptStarted = $false
$DownloadPath = ""
$ChecksumPath = ""
Start-Transcript -Path $LogPath -Append | Out-Null
$TranscriptStarted = $true
try {
  Start-Sleep -Seconds $DelaySeconds

  $UsePrebuilt = ![string]::IsNullOrWhiteSpace($BinaryUrl)
  if ($UsePrebuilt) {
    foreach ($Url in @($BinaryUrl, $ChecksumUrl)) {
      $parsedUrl = $null
      if (
        [string]::IsNullOrWhiteSpace($Url) -or
        ![Uri]::TryCreate($Url, [UriKind]::Absolute, [ref]$parsedUrl) -or
        $parsedUrl.Scheme -ne "https"
      ) {
        throw "prebuilt update URLs must use HTTPS"
      }
    }
    if (
      [string]::IsNullOrWhiteSpace($AssetName) -or
      [IO.Path]::GetFileName($AssetName) -ne $AssetName
    ) {
      throw "AssetName must be a plain file name"
    }

    $DownloadPath = Join-Path $InstallDir ".update-download-$PID.exe"
    $ChecksumPath = Join-Path $InstallDir ".update-checksums-$PID.txt"
    Invoke-WebRequest -UseBasicParsing -Uri $BinaryUrl -OutFile $DownloadPath
    Invoke-WebRequest -UseBasicParsing -Uri $ChecksumUrl -OutFile $ChecksumPath

    $ExpectedHash = Get-Content -LiteralPath $ChecksumPath |
      ForEach-Object {
        if ($_ -match '^([0-9A-Fa-f]{64})\s+\*?(.+)$' -and $Matches[2] -eq $AssetName) {
          $Matches[1].ToUpperInvariant()
        }
      } |
      Select-Object -First 1
    if ([string]::IsNullOrWhiteSpace($ExpectedHash)) {
      throw "checksum manifest does not contain the requested asset"
    }
    $ActualHash = (Get-FileHash -LiteralPath $DownloadPath -Algorithm SHA256).Hash
    if ($ActualHash -ne $ExpectedHash) {
      throw "downloaded binary checksum mismatch"
    }
    $NewExe = $DownloadPath
  } else {
    if (Test-Path -LiteralPath (Join-Path $SourceDir ".git")) {
      $actualUrl = (& git -C $SourceDir remote get-url origin).Trim()
      if ($LASTEXITCODE -ne 0) { throw "git remote get-url failed" }
      if ($actualUrl -ne $RepoUrl) { throw "git remote URL mismatch" }
      & git -C $SourceDir fetch origin $Branch
      if ($LASTEXITCODE -ne 0) { throw "git fetch failed" }
      $localHead = (& git -C $SourceDir rev-parse HEAD).Trim()
      if ($LASTEXITCODE -ne 0) { throw "git rev-parse HEAD failed" }
      $remoteHead = (& git -C $SourceDir rev-parse "origin/$Branch").Trim()
      if ($LASTEXITCODE -ne 0) { throw "git rev-parse origin failed" }
      if ($localHead -eq $remoteHead) {
        Write-Host "already up to date"
        exit 0
      }
      & git -C $SourceDir reset --hard "origin/$Branch"
      if ($LASTEXITCODE -ne 0) { throw "git reset failed" }
    } else {
      if (Test-Path -LiteralPath $SourceDir) {
        Remove-Item -LiteralPath $SourceDir -Recurse -Force
      }
      & git clone --branch $Branch --single-branch $RepoUrl $SourceDir
      if ($LASTEXITCODE -ne 0) { throw "git clone failed" }
    }

    if ($env:VERIFY_GIT_SIGNATURES -eq "1") {
      & git -C $SourceDir verify-commit HEAD
      if ($LASTEXITCODE -ne 0) { throw "git commit signature verification failed" }
    }

    & cargo build --release --manifest-path (Join-Path $SourceDir "Cargo.toml")
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

    $NewExe = Join-Path $SourceDir "target\release\distributed-watchdog.exe"
  }
  $InstallExe = Join-Path $InstallDir "distributed-watchdog.exe"
  $StopScript = Join-Path $InstallDir "stop-agent.ps1"
  $InstallScript = Join-Path $InstallDir "install-and-start.ps1"
  $HasStopScript = Test-Path -LiteralPath $StopScript -PathType Leaf
  $HasInstallScript = Test-Path -LiteralPath $InstallScript -PathType Leaf

  if ($HasStopScript) {
    & $StopScript -InstallDir $InstallDir -TaskName $TaskName
  }

  Copy-Item -LiteralPath $NewExe -Destination $InstallExe -Force
  if (!$UsePrebuilt) {
    Copy-Item -Path (Join-Path $SourceDir "deploy\windows\*.ps1") -Destination $InstallDir -Force
  }

  if ($HasInstallScript) {
    & $InstallScript -InstallDir $InstallDir -TaskName $TaskName
  } else {
    & (Join-Path $InstallDir "start-agent.ps1") -InstallDir $InstallDir
  }
} finally {
  foreach ($TemporaryPath in @($DownloadPath, $ChecksumPath)) {
    if (![string]::IsNullOrWhiteSpace($TemporaryPath)) {
      Remove-Item -LiteralPath $TemporaryPath -Force -ErrorAction SilentlyContinue
    }
  }
  Remove-Item -LiteralPath $LockPath -Force -ErrorAction SilentlyContinue
  if ($TranscriptStarted) {
    Stop-Transcript | Out-Null
  }
}
