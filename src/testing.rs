//! Test-only helpers shared across modules' unit tests.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::sync::{Mutex, Once, OnceLock};

struct WarnCollector;

static WARN_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

/// Every warn-level message captured since the collector was installed. The
/// buffer is process-global and shared by all tests, so assert by filtering
/// for a marker unique to the test rather than on the whole buffer.
pub(crate) fn warn_messages() -> &'static Mutex<Vec<String>> {
    WARN_MESSAGES.get_or_init(Mutex::default)
}

impl Log for WarnCollector {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Warn
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            warn_messages()
                .lock()
                .unwrap()
                .push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

/// Installs the process-wide warn collector. `log::set_logger` accepts one
/// logger per process, so every test that captures warns must install through
/// this shared entry point.
pub(crate) fn install_warn_collector() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&WarnCollector).expect("no other logger should be installed");
        log::set_max_level(LevelFilter::Warn);
    });
}
