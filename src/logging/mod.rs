use anyhow::Result;
use std::fs::OpenOptions;
use std::path::Path;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Initialize the tracing subscriber for logging.
///
/// When `log_dir` is provided, logs are written **only** to a file there.
/// Writing to stderr while the TUI owns the screen corrupts the display — log
/// lines appear at the bottom and push the panels up. Only when no log
/// directory is available do we fall back to stderr.
///
/// Uses the `JOT_LOG` environment variable or defaults to `info`.
pub fn init(log_dir: Option<&Path>) -> Result<()> {
    let filter = EnvFilter::try_from_env("JOT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    let (file_layer, stderr_layer) = match log_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            let log_path = dir.join("jot-down.log");
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)?;
            let layer = fmt::layer()
                .with_writer(file)
                .with_ansi(false)
                .with_target(true);
            (Some(layer), None)
        }
        None => {
            let layer = fmt::layer().with_writer(std::io::stderr).with_target(true);
            (None, Some(layer))
        }
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()?;

    Ok(())
}

/// Install a panic hook that restores terminal state and logs the panic
/// before propagating.
pub fn install_panic_hook() {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Try to restore terminal
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );

        tracing::error!("Panic: {}", info);
        prev_hook(info);
    }));
}
