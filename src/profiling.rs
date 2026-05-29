use std::sync::{
    LazyLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

static PROFILE_START: LazyLock<Instant> = LazyLock::new(Instant::now);
static PROFILE_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn init() {
    let _ = *PROFILE_START;
}

pub fn set_enabled(enabled: bool) {
    PROFILE_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    PROFILE_ENABLED.load(Ordering::Relaxed)
}

pub fn mark(event: impl AsRef<str>) {
    if enabled() {
        eprintln!(
            "{:5} ms time elapsed: {}",
            PROFILE_START.elapsed().as_millis(),
            event.as_ref()
        );
    }
}

pub fn mark_detail(event: impl AsRef<str>, detail: impl AsRef<str>) {
    if enabled() {
        eprintln!(
            "{:5} ms time elapsed: {} ({})",
            PROFILE_START.elapsed().as_millis(),
            event.as_ref(),
            detail.as_ref()
        );
    }
}

pub fn scope(event: &'static str) -> ProfileScope {
    ProfileScope {
        event,
        start: Instant::now(),
        enabled: enabled(),
        detail: None,
    }
}

pub fn scope_detail(event: &'static str, detail: impl Into<String>) -> ProfileScope {
    ProfileScope {
        event,
        start: Instant::now(),
        enabled: enabled(),
        detail: Some(detail.into()),
    }
}

pub struct ProfileScope {
    event: &'static str,
    start: Instant,
    enabled: bool,
    detail: Option<String>,
}

impl Drop for ProfileScope {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }

        match &self.detail {
            Some(detail) => eprintln!(
                "{:5} ms time elapsed: {} completed in {} ms ({})",
                PROFILE_START.elapsed().as_millis(),
                self.event,
                self.start.elapsed().as_millis(),
                detail
            ),
            None => eprintln!(
                "{:5} ms time elapsed: {} completed in {} ms",
                PROFILE_START.elapsed().as_millis(),
                self.event,
                self.start.elapsed().as_millis()
            ),
        }
    }
}
