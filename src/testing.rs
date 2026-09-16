//! Test-only helpers shared across modules' unit tests.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::sync::{Mutex, Once, OnceLock};

struct TestLogCollector;

static WARN_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static DEBUG_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

/// Debug capture is restricted to the hooks module so the buffer is not
/// flooded by debug output from the rest of the crate. Matching is
/// module-boundary-aware: only this exact module or its `::` submodules
/// qualify, so a sibling like `hooks_registry` would not.
const DEBUG_TARGET_PREFIX: &str = concat!(env!("CARGO_CRATE_NAME"), "::hooks");

fn captures_warn(metadata: &Metadata) -> bool {
    metadata.level() <= Level::Warn
}

fn captures_hooks_debug(metadata: &Metadata) -> bool {
    if metadata.level() != Level::Debug {
        return false;
    }
    let target = metadata.target();
    target == DEBUG_TARGET_PREFIX
        || target
            .strip_prefix(DEBUG_TARGET_PREFIX)
            .is_some_and(|rest| rest.starts_with("::"))
}

/// Every warn- or error-level message captured since the collector was
/// installed. The buffer is process-global and shared by all tests, so assert
/// by filtering for a marker unique to the test rather than on the whole
/// buffer.
pub(crate) fn warn_messages() -> &'static Mutex<Vec<String>> {
    WARN_MESSAGES.get_or_init(Mutex::default)
}

/// Every debug-level message from the hooks module captured since the
/// collector was installed. Same marker-filtering discipline as
/// [`warn_messages`].
pub(crate) fn debug_messages() -> &'static Mutex<Vec<String>> {
    DEBUG_MESSAGES.get_or_init(Mutex::default)
}

impl Log for TestLogCollector {
    fn enabled(&self, metadata: &Metadata) -> bool {
        captures_warn(metadata) || captures_hooks_debug(metadata)
    }

    // A test that panics while holding a buffer poisons its mutex; the logger
    // recovers via `into_inner` so every later test can still log.
    fn log(&self, record: &Record) {
        if captures_warn(record.metadata()) {
            warn_messages()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(record.args().to_string());
        } else if captures_hooks_debug(record.metadata()) {
            debug_messages()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

/// Installs the process-wide log collector. `log::set_logger` accepts one
/// logger per process, so every test that captures warns or hooks debug
/// output must install through this shared entry point. The max level is
/// Debug so the hooks debug capture sees its records; warn capture is
/// unaffected.
pub(crate) fn install_warn_collector() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&TestLogCollector).expect("no other logger should be installed");
        log::set_max_level(LevelFilter::Debug);
    });
}
