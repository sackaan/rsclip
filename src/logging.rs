//! Thread-safe ring buffer of log entries displayed in the GUI console.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

const MAX_LOG_LINES: usize = 500;

#[derive(Clone)]
pub struct LogEntry {
    pub level: log::Level,
    pub message: String,
    pub timestamp: String,
}

pub struct GuiLogger {
    lines: Mutex<VecDeque<LogEntry>>,
    level: log::LevelFilter,
    generation: AtomicU64,
}

impl GuiLogger {
    pub fn new(level: log::LevelFilter) -> Self {
        Self {
            lines: Mutex::new(VecDeque::with_capacity(MAX_LOG_LINES)),
            level,
            generation: AtomicU64::new(0),
        }
    }

    /// Returns log entries only if new ones have been added since `last_gen`.
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
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
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

pub fn init_gui_logger(level: log::LevelFilter) {
    let logger = Box::leak(Box::new(GuiLogger::new(level)));
    GUI_LOGGER.set(logger).ok();
    log::set_logger(logger).ok();
    log::set_max_level(level);
}

pub fn gui_logger() -> Option<&'static GuiLogger> {
    GUI_LOGGER.get().copied()
}
