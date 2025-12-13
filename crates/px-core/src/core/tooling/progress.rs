use std::env;
use std::io::{self, IsTerminal, Write};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn progress_enabled() -> bool {
    match env::var("PX_PROGRESS") {
        Ok(value) => value != "0",
        Err(_) => io::stderr().is_terminal(),
    }
}

static OUTPUT_LOCK: Mutex<()> = Mutex::new(());
static MANAGER: OnceLock<ProgressManager> = OnceLock::new();

fn manager() -> &'static ProgressManager {
    MANAGER.get_or_init(ProgressManager::new)
}

fn clear_progress_line() {
    let _guard = OUTPUT_LOCK.lock().ok();
    let _ = io::stderr().write_all(b"\r\x1b[2K");
    let _ = io::stderr().flush();
}

#[derive(Clone)]
struct ProgressTask {
    id: u64,
    label: String,
    total: Option<usize>,
    current: usize,
    started_at: Instant,
}

struct ProgressManager {
    state: Mutex<ProgressState>,
}

struct ProgressState {
    next_id: u64,
    suspend_count: usize,
    tasks: Vec<ProgressTask>,
    renderer_started: bool,
}

impl ProgressManager {
    fn new() -> Self {
        Self {
            state: Mutex::new(ProgressState {
                next_id: 1,
                suspend_count: 0,
                tasks: Vec::new(),
                renderer_started: false,
            }),
        }
    }

    fn start_renderer(&self) {
        let mut state = self.state.lock().expect("progress lock");
        if state.renderer_started {
            return;
        }
        state.renderer_started = true;
        drop(state);

        thread::spawn(|| {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            const TICK: Duration = Duration::from_millis(80);
            const START_DELAY: Duration = Duration::from_millis(120);
            let mut idx = 0usize;
            let mut rendered = false;
            loop {
                let (suspended, task) = {
                    let state = manager().state.lock().expect("progress lock");
                    let suspended = state.suspend_count > 0 || !progress_enabled();
                    let task = state.tasks.last().cloned();
                    (suspended, task)
                };

                if suspended || task.is_none() {
                    if rendered {
                        clear_progress_line();
                        rendered = false;
                    }
                    thread::sleep(TICK);
                    continue;
                }

                let task = task.expect("task is_some");
                let elapsed = Instant::now().saturating_duration_since(task.started_at);
                if elapsed < START_DELAY {
                    if rendered {
                        clear_progress_line();
                        rendered = false;
                    }
                    thread::sleep(TICK);
                    continue;
                }

                let frame = FRAMES[idx % FRAMES.len()];
                idx = idx.wrapping_add(1);
                let line = if let Some(total) = task.total {
                    let current = task.current.min(total);
                    format!(
                        "\r\x1b[2Kpx ▸ {} [{current}/{total}] {frame}",
                        task.label
                    )
                } else {
                    format!("\r\x1b[2Kpx ▸ {} {frame}", task.label)
                };
                {
                    let _guard = OUTPUT_LOCK.lock().ok();
                    let _ = io::stderr().write_all(line.as_bytes());
                    let _ = io::stderr().flush();
                }
                rendered = true;
                thread::sleep(TICK);
            }
        });
    }

    fn push_task(&self, label: String, total: Option<usize>) -> u64 {
        let mut state = self.state.lock().expect("progress lock");
        let id = state.next_id;
        state.next_id = state.next_id.saturating_add(1);
        state.tasks.push(ProgressTask {
            id,
            label,
            total,
            current: 0,
            started_at: Instant::now(),
        });
        id
    }

    fn update_current(&self, id: u64, delta: usize) {
        let mut state = self.state.lock().expect("progress lock");
        if let Some(task) = state.tasks.iter_mut().find(|task| task.id == id) {
            task.current = task.current.saturating_add(delta);
        }
    }

    fn remove_task(&self, id: u64) {
        let mut state = self.state.lock().expect("progress lock");
        if let Some(pos) = state.tasks.iter().position(|task| task.id == id) {
            state.tasks.remove(pos);
        }
    }

    fn suspend(&self) {
        let mut state = self.state.lock().expect("progress lock");
        state.suspend_count = state.suspend_count.saturating_add(1);
    }

    fn resume(&self) {
        let mut state = self.state.lock().expect("progress lock");
        state.suspend_count = state.suspend_count.saturating_sub(1);
    }
}

pub(crate) struct ProgressSuspendGuard {
    enabled: bool,
}

impl ProgressSuspendGuard {
    pub(crate) fn new() -> Self {
        if !progress_enabled() {
            return Self { enabled: false };
        }
        manager().suspend();
        clear_progress_line();
        Self { enabled: true }
    }
}

impl Drop for ProgressSuspendGuard {
    fn drop(&mut self) {
        if self.enabled {
            manager().resume();
        }
    }
}

pub struct ProgressReporter {
    id: Option<u64>,
    enabled: bool,
}

impl ProgressReporter {
    pub fn spinner(label: impl Into<String>) -> Self {
        Self::start(label, None)
    }

    pub fn bar(label: impl Into<String>, total: usize) -> Self {
        if total == 0 {
            return Self::spinner(label);
        }
        Self::start(label, Some(total))
    }

    fn start(label: impl Into<String>, total: Option<usize>) -> Self {
        let label = label.into();
        if !progress_enabled() {
            return Self {
                id: None,
                enabled: false,
            };
        }
        manager().start_renderer();
        let id = manager().push_task(label, total);
        Self {
            id: Some(id),
            enabled: true,
        }
    }

    pub fn increment(&self) {
        if self.enabled {
            if let Some(id) = self.id {
                manager().update_current(id, 1);
            }
        }
    }

    pub fn finish(mut self, message: impl Into<String>) {
        self.stop();
        eprintln!("px ▸ {}", message.into());
    }

    fn stop(&mut self) {
        if self.enabled {
            if let Some(id) = self.id.take() {
                manager().remove_task(id);
                clear_progress_line();
            }
            self.enabled = false;
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
        self.stop();
    }
}
