use crate::utils::IS_STDOUT_TERMINAL;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::LazyLock;
use std::time::Duration;

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

static SPINNER_STYLE: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{spinner} [{prefix}] {msg} ({elapsed})")
        .expect("valid template")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", ""])
});

pub(super) struct BranchProgressTracker {
    multi: Option<MultiProgress>,
}

impl BranchProgressTracker {
    pub fn new() -> Self {
        if *IS_STDOUT_TERMINAL {
            Self {
                multi: Some(MultiProgress::new()),
            }
        } else {
            Self { multi: None }
        }
    }

    pub fn add_branch(&self, label: &str) -> BranchProgressHandle {
        let Some(multi) = &self.multi else {
            return BranchProgressHandle::disabled();
        };
        let bar = multi.add(ProgressBar::new_spinner());
        bar.set_style(SPINNER_STYLE.clone());
        bar.set_prefix(label.to_string());
        bar.set_message("running…");
        bar.enable_steady_tick(Duration::from_millis(80));
        BranchProgressHandle { bar: Some(bar) }
    }
}

pub(super) struct BranchProgressHandle {
    bar: Option<ProgressBar>,
}

impl BranchProgressHandle {
    pub fn disabled() -> Self {
        Self { bar: None }
    }

    pub fn complete(self) {
        if let Some(bar) = self.bar {
            bar.finish_with_message(format!("{GREEN}✓ done{RESET}"));
        }
    }

    pub fn fail(self, err: &str) {
        if let Some(bar) = self.bar {
            let truncated = if err.len() > 80 {
                let mut s = err[..80].to_string();
                s.push('…');
                s
            } else {
                err.to_string()
            };
            bar.finish_with_message(format!("{RED}✗ failed {RESET} — {truncated}"));
        }
    }
}
