//! qcapture-widget: GUI launcher for the floating control widget.
//!
//! Same widget as `qcapture widget`, but built as a GUI-subsystem binary:
//! no console window pops up on Windows, and logs go to a rotating file
//! instead of stderr:
//! `%APPDATA%\QCapture\logs\qcapture-widget-<timestamp>.log`
//! (`~/.config/qcapture/logs` elsewhere, newest 10 kept).
//! Fatal startup errors surface in a native message dialog (there is no
//! console to print to).

#![cfg_attr(windows, windows_subsystem = "windows")]

/// Open (creating dirs) the log file for this launch, pruning old ones.
fn open_log_file() -> std::io::Result<std::path::PathBuf> {
    #[cfg(windows)]
    let dir = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("QCapture")
        .join("logs");
    #[cfg(not(windows))]
    let dir = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(".config")
        .join("qcapture")
        .join("logs");
    std::fs::create_dir_all(&dir)?;
    // Keep the newest 10 logs; launches are cheap, disks are not infinite.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut logs: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("qcapture-widget-") && n.ends_with(".log"))
            })
            .collect();
        logs.sort();
        for stale in logs.iter().rev().skip(10) {
            let _ = std::fs::remove_file(stale);
        }
    }
    let name = format!(
        "qcapture-widget-{}.log",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    );
    let path = dir.join(name);
    std::fs::File::create(&path)?;
    Ok(path)
}

fn fatal(message: String) -> ! {
    rfd::MessageDialog::new()
        .set_title("QCapture failed to start")
        .set_description(format!("{message}\n\nSee the latest log for details."))
        .set_level(rfd::MessageLevel::Error)
        .show();
    std::process::exit(1);
}

fn main() {
    let log_path = open_log_file().unwrap_or_else(|e| {
        fatal(format!("cannot open log file: {e}"));
    });
    let log_file = std::fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|e| fatal(format!("cannot open log file {}: {e}", log_path.display())));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(log_file))
        .init();
    tracing::info!("logging to {}", log_path.display());

    if let Err(e) = qcapture_ui::widget::run() {
        tracing::error!("widget failed: {e:#}");
        fatal(format!("{e:#}"));
    }
}
