use crate::utils::IS_STDOUT_TERMINAL;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

static SPINNER_STYLE: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{spinner} [{prefix}] {msg}")
        .expect("valid template")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", ""])
});

// Manages a set of per-branch spinners drawn side-by-side via indicatif's
// `MultiProgress`. Created at the start of a multi-branch graph super-step
// (or map sub-branch fan-out) and torn down at the join.
//
// When stdout isn't a terminal (CI, piped output), the tracker becomes a
// no-op — `add_branch` returns a disabled handle whose methods do nothing.
// This keeps machine-piped graph runs free of spinner garbage in their
// captured output.
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
        BranchProgressHandle {
            bar: Some(bar),
            started: Instant::now(),
        }
    }

    pub fn clear(&self) {
        if let Some(multi) = &self.multi {
            let _ = multi.clear();
        }
    }
}

pub(super) struct BranchProgressHandle {
    bar: Option<ProgressBar>,
    started: Instant,
}

impl BranchProgressHandle {
    fn disabled() -> Self {
        Self {
            bar: None,
            started: Instant::now(),
        }
    }

    pub fn complete(self) {
        if let Some(bar) = self.bar {
            let elapsed = self.started.elapsed();
            bar.finish_with_message(format!("✓ done ({:.1}s)", elapsed.as_secs_f64()));
        }
    }

    pub fn fail(self, err: &str) {
        if let Some(bar) = self.bar {
            let elapsed = self.started.elapsed();
            let truncated = if err.len() > 80 {
                let mut s = err[..80].to_string();
                s.push('…');
                s
            } else {
                err.to_string()
            };
            bar.finish_with_message(format!(
                "✗ failed ({:.1}s) — {}",
                elapsed.as_secs_f64(),
                truncated
            ));
        }
    }
}
