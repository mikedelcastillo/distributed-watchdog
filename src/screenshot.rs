use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, bail};
use chrono::Utc;
use tokio::{process::Command, time::timeout};

#[derive(Debug, Clone)]
pub struct Screenshot {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub content_type: &'static str,
}

static NEXT_SCREENSHOT_ID: AtomicU64 = AtomicU64::new(1);

pub async fn capture() -> anyhow::Result<Screenshot> {
    let path = temp_path();
    let result = capture_to_path(&path).await;
    match result {
        Ok(()) => {
            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("failed to read screenshot {}", path.display()))?;
            let _ = fs::remove_file(&path);
            Ok(Screenshot {
                bytes,
                filename: format!("screenshot-{}.png", Utc::now().format("%Y%m%d-%H%M%S")),
                content_type: "image/png",
            })
        }
        Err(err) => {
            let _ = fs::remove_file(&path);
            Err(err)
        }
    }
}

async fn capture_to_path(path: &Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        capture_windows(path).await
    }

    #[cfg(target_os = "macos")]
    {
        capture_macos(path).await
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        capture_linux(path).await
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        bail!("screenshots are not implemented on this platform");
    }
}

#[cfg(windows)]
async fn capture_windows(path: &Path) -> anyhow::Result<()> {
    let path = path.display().to_string().replace('\'', "''");
    let script = format!(
        r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$screens = [System.Windows.Forms.Screen]::AllScreens
if ($screens.Count -eq 0) {{ throw "no screens available" }}
$left = ($screens | ForEach-Object {{ $_.Bounds.Left }} | Measure-Object -Minimum).Minimum
$top = ($screens | ForEach-Object {{ $_.Bounds.Top }} | Measure-Object -Minimum).Minimum
$right = ($screens | ForEach-Object {{ $_.Bounds.Right }} | Measure-Object -Maximum).Maximum
$bottom = ($screens | ForEach-Object {{ $_.Bounds.Bottom }} | Measure-Object -Maximum).Maximum
$width = [int]($right - $left)
$height = [int]($bottom - $top)
if ($width -le 0 -or $height -le 0) {{ throw "invalid screen bounds" }}
$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
try {{
  $graphics.CopyFromScreen([int]$left, [int]$top, 0, 0, $bitmap.Size)
  $bitmap.Save('{path}', [System.Drawing.Imaging.ImageFormat]::Png)
}} finally {{
  $graphics.Dispose()
  $bitmap.Dispose()
}}
"#
    );

    run_command(
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &script,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
        "PowerShell screenshot",
    )
    .await
}

#[cfg(target_os = "macos")]
async fn capture_macos(path: &Path) -> anyhow::Result<()> {
    run_command(
        Command::new("screencapture")
            .arg("-x")
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
        "screencapture",
    )
    .await
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn capture_linux(path: &Path) -> anyhow::Result<()> {
    let attempts: Vec<(&str, Vec<String>)> = vec![
        ("grim", vec![path.display().to_string()]),
        (
            "gnome-screenshot",
            vec!["-f".to_string(), path.display().to_string()],
        ),
        (
            "spectacle",
            vec![
                "-b".to_string(),
                "-n".to_string(),
                "-o".to_string(),
                path.display().to_string(),
            ],
        ),
        ("scrot", vec![path.display().to_string()]),
        ("maim", vec![path.display().to_string()]),
        (
            "import",
            vec![
                "-window".to_string(),
                "root".to_string(),
                path.display().to_string(),
            ],
        ),
    ];

    let mut errors = Vec::new();
    for (program, args) in attempts {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match run_command(&mut command, program).await {
            Ok(()) if path.exists() => return Ok(()),
            Ok(()) => errors.push(format!("{program}: did not create screenshot")),
            Err(err) => errors.push(format!("{program}: {err}")),
        }
    }

    bail!(
        "no Linux screenshot command succeeded. Install grim, gnome-screenshot, spectacle, scrot, maim, or ImageMagick import; display session permissions may also be required. First error: {}",
        errors
            .first()
            .cloned()
            .unwrap_or_else(|| "none".to_string())
    )
}

async fn run_command(command: &mut Command, label: &str) -> anyhow::Result<()> {
    let output = timeout(Duration::from_secs(10), command.output())
        .await
        .with_context(|| format!("{label} timed out"))?
        .with_context(|| format!("failed to run {label}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{} failed: {}", label, stderr.trim());
    }

    Ok(())
}

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "distributed-watchdog-screenshot-{}-{}-{}.png",
        std::process::id(),
        Utc::now().timestamp_millis(),
        NEXT_SCREENSHOT_ID.fetch_add(1, Ordering::Relaxed)
    ))
}
