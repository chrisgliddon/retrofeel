use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bevy::prelude::{Res, Resource};
use env_logger::{Builder, Env, Target};
use retrofeel_types::RetroFeelConfig;

const DEFAULT_LOG_FILTER: &str =
    "retrofeel=debug,libretro_host=debug,retrofeel_backend=debug,retrofeel_export=info,bevy=warn,wgpu=warn,naga=warn";
const VERBOSE_LOG_FILTER: &str =
    "retrofeel=debug,libretro_host=debug,retrofeel_backend=debug,retrofeel_export=debug,bevy=warn,wgpu=warn,naga=warn";
const WATCHDOG_STALE_MS: u64 = 1_500;
const WATCHDOG_LOG_EVERY_MS: u64 = 2_000;

#[derive(Debug, Clone)]
pub struct LoggingOptions {
    pub verbose: bool,
    pub log_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LoggingInit {
    pub log_path: PathBuf,
}

pub fn init_logging(options: LoggingOptions) -> io::Result<LoggingInit> {
    let log_path = options.log_file.unwrap_or_else(default_log_path);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let writer = TeeWriter::new(io::stderr(), file);
    let env = Env::default().default_filter_or(default_log_filter(options.verbose));
    let mut builder = Builder::from_env(env);
    builder.target(Target::Pipe(Box::new(writer)));
    builder
        .try_init()
        .map_err(|error| io::Error::other(format!("failed to initialize logger: {error}")))?;

    eprintln!("retrofeel log: {}", log_path.display());
    log::info!("retrofeel log: {}", log_path.display());
    Ok(LoggingInit { log_path })
}

pub fn default_log_path() -> PathBuf {
    let cache_dir = RetroFeelConfig::project_dirs()
        .ok()
        .map(|dirs| dirs.cache_dir().to_path_buf());
    default_log_path_from_cache_dir(cache_dir)
}

fn default_log_path_from_cache_dir(cache_dir: Option<PathBuf>) -> PathBuf {
    cache_dir
        .unwrap_or_else(|| std::env::temp_dir().join("retrofeel"))
        .join("logs")
        .join("retrofeel.log")
}

fn default_log_filter(verbose: bool) -> &'static str {
    if verbose {
        VERBOSE_LOG_FILTER
    } else {
        DEFAULT_LOG_FILTER
    }
}

pub struct TeeWriter<L, R> {
    left: L,
    right: R,
}

impl<L, R> TeeWriter<L, R> {
    pub fn new(left: L, right: R) -> Self {
        Self { left, right }
    }
}

impl<L: Write, R: Write> Write for TeeWriter<L, R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.left.write_all(buf)?;
        self.right.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.left.flush()?;
        self.right.flush()
    }
}

pub fn time_block<T>(label: impl Into<String>, f: impl FnOnce() -> T) -> T {
    let timing = Timing::start(label);
    let result = f();
    timing.finish();
    result
}

pub struct Timing {
    label: String,
    start: Instant,
}

impl Timing {
    pub fn start(label: impl Into<String>) -> Self {
        let label = label.into();
        log::debug!("timing.begin name=\"{label}\"");
        Self {
            label,
            start: Instant::now(),
        }
    }

    pub fn finish(self) -> Duration {
        let elapsed = self.start.elapsed();
        log_elapsed("timing.end", &self.label, elapsed);
        elapsed
    }
}

pub fn log_action_end(
    action_id: u64,
    state: &str,
    action: &str,
    next_state: &str,
    elapsed: Duration,
) {
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    if elapsed >= Duration::from_millis(1_000) {
        log::error!(
            "ui.action.end action_id={action_id} state=\"{state}\" action=\"{action}\" next_state=\"{next_state}\" elapsed_ms={elapsed_ms:.1}"
        );
    } else if elapsed >= Duration::from_millis(250) {
        log::warn!(
            "ui.action.end action_id={action_id} state=\"{state}\" action=\"{action}\" next_state=\"{next_state}\" elapsed_ms={elapsed_ms:.1}"
        );
    } else {
        log::debug!(
            "ui.action.end action_id={action_id} state=\"{state}\" action=\"{action}\" next_state=\"{next_state}\" elapsed_ms={elapsed_ms:.1}"
        );
    }
}

fn log_elapsed(event: &str, label: &str, elapsed: Duration) {
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    if elapsed >= Duration::from_millis(1_000) {
        log::error!("{event} name=\"{label}\" elapsed_ms={elapsed_ms:.1}");
    } else if elapsed >= Duration::from_millis(250) {
        log::warn!("{event} name=\"{label}\" elapsed_ms={elapsed_ms:.1}");
    } else {
        log::debug!("{event} name=\"{label}\" elapsed_ms={elapsed_ms:.1}");
    }
}

#[derive(Resource, Default)]
#[allow(dead_code)]
pub struct UiActionIds {
    next: u64,
}

impl UiActionIds {
    #[allow(dead_code)]
    pub fn next(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
}

#[derive(Resource, Clone)]
pub struct Diagnostics {
    inner: Arc<DiagnosticsInner>,
}

struct DiagnosticsInner {
    started: AtomicBool,
    heartbeat_ms: AtomicU64,
    active_action: Mutex<Option<ActiveAction>>,
    /// Set while a native file picker (rfd async dialog) is open. The watchdog
    /// treats the main-thread heartbeat going stale while this is set as
    /// expected — the main thread is parked in a Cocoa modal session that the
    /// user is interacting with, not actually frozen — so it skips the
    /// `main_thread.stalled` log until the picker closes.
    picker_open: AtomicBool,
}

#[derive(Clone)]
struct ActiveAction {
    id: u64,
    label: String,
    state: String,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            inner: Arc::new(DiagnosticsInner {
                started: AtomicBool::new(false),
                heartbeat_ms: AtomicU64::new(now_ms()),
                active_action: Mutex::new(None),
                picker_open: AtomicBool::new(false),
            }),
        }
    }
}

impl Diagnostics {
    pub fn start_watchdog(&self) {
        self.heartbeat();
        if self.inner.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        match thread::Builder::new()
            .name("retrofeel-main-watchdog".into())
            .spawn(move || watchdog_loop(inner))
        {
            Ok(_) => log::debug!("main_thread.watchdog.started"),
            Err(error) => {
                self.inner.started.store(false, Ordering::Release);
                log::error!("main_thread.watchdog.start_failed error=\"{error}\"");
            }
        }
    }

    pub fn heartbeat(&self) {
        self.inner.heartbeat_ms.store(now_ms(), Ordering::Release);
    }

    pub fn begin_action(&self, id: u64, label: String, state: String) {
        if let Ok(mut active) = self.inner.active_action.lock() {
            *active = Some(ActiveAction { id, label, state });
        }
    }

    /// Mark whether a native file picker is currently open. The watchdog reads
    /// this to suppress false-positive `main_thread.stalled` logs that would
    /// otherwise fire while the main thread is parked in a blocking modal
    /// session (e.g. an rfd folder/file picker the user is interacting with).
    pub fn set_picker_open(&self, open: bool) {
        self.inner.picker_open.store(open, Ordering::Release);
    }

    pub fn end_action(&self, id: u64) {
        if let Ok(mut active) = self.inner.active_action.lock() {
            if active.as_ref().is_some_and(|action| action.id == id) {
                *active = None;
            }
        }
    }
}

pub fn start_watchdog_system(diagnostics: Res<Diagnostics>) {
    diagnostics.start_watchdog();
}

pub fn heartbeat_system(diagnostics: Res<Diagnostics>) {
    diagnostics.heartbeat();
}

fn watchdog_loop(inner: Arc<DiagnosticsInner>) {
    let mut last_log_ms = 0;
    loop {
        thread::sleep(Duration::from_millis(500));
        let now = now_ms();
        let heartbeat = inner.heartbeat_ms.load(Ordering::Acquire);
        let stale_ms = now.saturating_sub(heartbeat);
        if stale_ms <= WATCHDOG_STALE_MS {
            last_log_ms = 0;
            continue;
        }
        // A native file picker (rfd) parks the main thread in a Cocoa modal
        // session the user is interacting with. The stale heartbeat is
        // expected in that case, so don't log a stall.
        if inner.picker_open.load(Ordering::Acquire) {
            last_log_ms = 0;
            continue;
        }
        if last_log_ms != 0 && now.saturating_sub(last_log_ms) < WATCHDOG_LOG_EVERY_MS {
            continue;
        }
        last_log_ms = now;
        let active = inner
            .active_action
            .lock()
            .ok()
            .and_then(|action| action.clone());
        if let Some(action) = active {
            log::error!(
                "main_thread.stalled stale_ms={stale_ms} action_id={} action=\"{}\" state=\"{}\"",
                action.id,
                action.label,
                action.state
            );
        } else {
            log::error!("main_thread.stalled stale_ms={stale_ms} action_id=none action=none");
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    struct SharedSink {
        bytes: Arc<Mutex<Vec<u8>>>,
        flushes: Arc<AtomicUsize>,
    }

    impl SharedSink {
        fn bytes(&self) -> Vec<u8> {
            self.bytes.lock().unwrap().clone()
        }

        fn flushes(&self) -> usize {
            self.flushes.load(Ordering::Acquire)
        }
    }

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    #[test]
    fn default_log_path_falls_back_without_project_dirs() {
        let path = default_log_path_from_cache_dir(None);
        assert_eq!(
            path,
            std::env::temp_dir()
                .join("retrofeel")
                .join("logs")
                .join("retrofeel.log")
        );
    }

    #[test]
    fn tee_writer_writes_and_flushes_both_sinks() {
        let left = SharedSink::default();
        let right = SharedSink::default();
        let mut writer = TeeWriter::new(left.clone(), right.clone());

        writer.write_all(b"abc").unwrap();
        writer.flush().unwrap();

        assert_eq!(left.bytes(), b"abc");
        assert_eq!(right.bytes(), b"abc");
        assert_eq!(left.flushes(), 1);
        assert_eq!(right.flushes(), 1);
    }
}
