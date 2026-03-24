use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const CLEAR_LINE: &str = "\r\x1b[2K";
const SPINNER_FRAMES: &[&str] = &["-", "\\", "|", "/"];

pub struct StatusLine {
    inner: Arc<StatusLineInner>,
}

struct StatusLineInner {
    enabled: bool,
    owners: AtomicUsize,
    frame_index: AtomicUsize,
    stop: AtomicBool,
    suspend_count: AtomicUsize,
    message: Mutex<String>,
    render_thread: Mutex<Option<JoinHandle<()>>>,
}

pub struct StatusLineSuspendGuard {
    status_line: StatusLine,
}

impl StatusLine {
    pub fn new(initial_message: impl Into<String>) -> Self {
        let enabled = atty::is(atty::Stream::Stderr);
        let inner = Arc::new(StatusLineInner {
            enabled,
            owners: AtomicUsize::new(1),
            frame_index: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            suspend_count: AtomicUsize::new(0),
            message: Mutex::new(String::new()),
            render_thread: Mutex::new(None),
        });

        if enabled {
            let thread_inner = Arc::clone(&inner);
            let handle = thread::spawn(move || run_status_line(thread_inner));
            if let Ok(mut slot) = inner.render_thread.lock() {
                *slot = Some(handle);
            }
        }

        let status_line = Self { inner };
        status_line.update(initial_message);
        status_line
    }

    pub fn update(&self, message: impl Into<String>) {
        if !self.inner.enabled {
            return;
        }
        if let Ok(mut slot) = self.inner.message.lock() {
            *slot = sanitize_message(message.into());
        }
    }

    pub fn suspend(&self) -> StatusLineSuspendGuard {
        if self.inner.enabled {
            self.inner.suspend_count.fetch_add(1, Ordering::Relaxed);
            clear_status_line();
        }
        StatusLineSuspendGuard {
            status_line: self.clone(),
        }
    }
}

impl Clone for StatusLine {
    fn clone(&self) -> Self {
        self.inner.owners.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for StatusLine {
    fn drop(&mut self) {
        if self.inner.owners.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }

        if self.inner.enabled {
            self.inner.stop.store(true, Ordering::Release);
            if let Ok(mut slot) = self.inner.render_thread.lock() {
                if let Some(handle) = slot.take() {
                    let _ = handle.join();
                }
            }
            clear_status_line();
        }
    }
}

impl Drop for StatusLineSuspendGuard {
    fn drop(&mut self) {
        if self.status_line.inner.enabled {
            self.status_line
                .inner
                .suspend_count
                .fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn run_status_line(inner: Arc<StatusLineInner>) {
    loop {
        if inner.stop.load(Ordering::Acquire) {
            break;
        }

        if inner.suspend_count.load(Ordering::Relaxed) > 0 {
            clear_status_line();
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        let message = inner
            .message
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default();

        if message.is_empty() {
            clear_status_line();
        } else {
            let frame = SPINNER_FRAMES
                [inner.frame_index.fetch_add(1, Ordering::Relaxed) % SPINNER_FRAMES.len()];
            write_status_line(&format!("{} {}", frame, message));
        }

        thread::sleep(Duration::from_millis(80));
    }

    clear_status_line();
}

fn sanitize_message(message: String) -> String {
    let line = message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn write_status_line(message: &str) {
    let mut stderr = io::stderr();
    let _ = write!(stderr, "{}{}", CLEAR_LINE, message);
    let _ = stderr.flush();
}

fn clear_status_line() {
    let mut stderr = io::stderr();
    let _ = write!(stderr, "{}", CLEAR_LINE);
    let _ = stderr.flush();
}
