use std::env;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub(crate) fn progress_enabled() -> bool {
    match env::var("PX_PROGRESS") {
        Ok(value) => value != "0",
        Err(_) => io::stderr().is_terminal(),
    }
}

pub(crate) struct ProgressReporter {
    current: Arc<AtomicUsize>,
    stop: Option<Arc<AtomicBool>>,
    handle: Option<thread::JoinHandle<()>>,
    enabled: bool,
}

impl ProgressReporter {
    pub(crate) fn spinner(label: impl Into<String>) -> Self {
        Self::start(label, None)
    }

    pub(crate) fn bar(label: impl Into<String>, total: usize) -> Self {
        if total == 0 {
            return Self::spinner(label);
        }
        Self::start(label, Some(total))
    }

    fn start(label: impl Into<String>, total: Option<usize>) -> Self {
        let label = label.into();
        if !progress_enabled() {
            return Self {
                current: Arc::new(AtomicUsize::new(0)),
                stop: None,
                handle: None,
                enabled: false,
            };
        }

        let stop = Arc::new(AtomicBool::new(false));
        let current = Arc::new(AtomicUsize::new(0));
        let thread_label = label.clone();
        let thread_total = total;
        let thread_stop = Arc::clone(&stop);
        let thread_current = Arc::clone(&current);
        let handle = thread::spawn(move || {
            ProgressReporter::run(&thread_label, thread_total, &thread_current, &thread_stop);
        });

        Self {
            current,
            stop: Some(stop),
            handle: Some(handle),
            enabled: true,
        }
    }

    pub(crate) fn increment(&self) {
        if self.enabled {
            self.current.fetch_add(1, AtomicOrdering::Relaxed);
        }
    }

    pub(crate) fn finish(mut self, message: impl Into<String>) {
        if self.enabled {
            if let Some(stop) = self.stop.take() {
                stop.store(true, AtomicOrdering::Relaxed);
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            let _ = io::stderr().write_all(b"\r\x1b[2K");
            let _ = io::stderr().flush();
        }
        eprintln!("px ▸ {}", message.into());
    }

    fn run(label: &str, total: Option<usize>, current: &Arc<AtomicUsize>, stop: &Arc<AtomicBool>) {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut idx = 0;
        while !stop.load(AtomicOrdering::Relaxed) {
            let frame = FRAMES[idx % FRAMES.len()];
            idx += 1;
            let line = if let Some(total) = total {
                let current = current.load(AtomicOrdering::Relaxed).min(total);
                format!("\r\x1b[2Kpx ▸ {label} [{current}/{total}] {frame}")
            } else {
                format!("\r\x1b[2Kpx ▸ {label} {frame}")
            };
            let _ = io::stderr().write_all(line.as_bytes());
            let _ = io::stderr().flush();
            thread::sleep(Duration::from_millis(80));
        }
    }
}

pub(crate) fn download_concurrency(total: usize) -> usize {
    let requested = env::var("PX_DOWNLOADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    let available = thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4)
        .max(1);
    let max_workers = requested.unwrap_or(available).clamp(1, 16);
    max_workers.min(total.max(1))
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        if self.enabled {
            if let Some(stop) = self.stop.take() {
                stop.store(true, AtomicOrdering::Relaxed);
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            let _ = io::stderr().write_all(b"\r\x1b[2K");
            let _ = io::stderr().flush();
        }
    }
}
