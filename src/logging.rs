//! Thread-safe ring buffer of log entries displayed in the GUI console.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

const MAX_LOG_LINES: usize = 500;

/// A single line stored in the in-memory log ring buffer.
#[derive(Clone)]
pub struct LogEntry {
    /// Original `log` crate level.
    pub level: log::Level,
    /// Formatted message body.
    pub message: String,
    /// `HH:MM:SS` local time when the log was recorded.
    pub timestamp: String,
}

/// Thread-safe ring buffer of recent log entries surfaced in the GUI console.
///
/// Bounded to [`MAX_LOG_LINES`]. A generation counter lets the GUI cheaply
/// detect when new entries have arrived without re-cloning every frame.
pub struct GuiLogger {
    lines: Mutex<VecDeque<LogEntry>>,
    level: log::LevelFilter,
    generation: AtomicU64,
}

impl GuiLogger {
    /// Construct an empty logger filtered to `level`.
    pub fn new(level: log::LevelFilter) -> Self {
        Self {
            lines: Mutex::new(VecDeque::with_capacity(MAX_LOG_LINES)),
            level,
            generation: AtomicU64::new(0),
        }
    }

    /// Return a snapshot of all entries iff new ones have been pushed since
    /// `last_gen`. Updates `last_gen` to the current generation on return.
    pub fn entries_if_changed(&self, last_gen: &mut u64) -> Option<Vec<LogEntry>> {
        let current = self.generation.load(Ordering::Relaxed);
        if current == *last_gen {
            return None;
        }
        *last_gen = current;
        Some(self.lines.lock().iter().cloned().collect())
    }
}

impl log::Log for GuiLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Skip noisy crates
        let target = record.target();
        if target.starts_with("wgpu")
            || target.starts_with("naga")
            || target.starts_with("eframe")
            || target.starts_with("egui")
        {
            return;
        }
        let entry = LogEntry {
            level: record.level(),
            message: format!("{}", record.args()),
            timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
        };
        let mut lines = self.lines.lock();
        if lines.len() >= MAX_LOG_LINES {
            lines.pop_front();
        }
        lines.push_back(entry);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn flush(&self) {}
}

static GUI_LOGGER: std::sync::OnceLock<&'static GuiLogger> = std::sync::OnceLock::new();

/// Install the GUI logger as the global `log` sink.
///
/// Safe to call multiple times; subsequent calls are no-ops. The logger is
/// leaked into `'static` storage on first call so it can be referenced by
/// the `log` facade for the lifetime of the process.
pub fn init_gui_logger(level: log::LevelFilter) {
    let logger = Box::leak(Box::new(GuiLogger::new(level)));
    GUI_LOGGER.set(logger).ok();
    log::set_logger(logger).ok();
    log::set_max_level(level);
}

/// Borrow the installed logger, if [`init_gui_logger`] has run.
pub fn gui_logger() -> Option<&'static GuiLogger> {
    GUI_LOGGER.get().copied()
}
