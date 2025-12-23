use std::sync::OnceLock;
use std::time::Instant;

fn timings_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("PX_TIMINGS")
            .ok()
            .map(|raw| {
                let value = raw.trim();
                !value.is_empty()
                    && !matches!(
                        value.to_ascii_lowercase().as_str(),
                        "0" | "false" | "no" | "off"
                    )
            })
            .unwrap_or(false)
    })
}

pub(crate) struct TimingGuard {
    label: &'static str,
    start: Instant,
}

impl TimingGuard {
    pub(crate) fn new(label: &'static str) -> Option<Self> {
        if timings_enabled() {
            Some(Self {
                label,
                start: Instant::now(),
            })
        } else {
            None
        }
    }
}

impl Drop for TimingGuard {
    fn drop(&mut self) {
        if !timings_enabled() {
            return;
        }
        let elapsed = self.start.elapsed();
        tracing::info!(
            px_timing = self.label,
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            "timing"
        );
    }
}
