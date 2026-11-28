//! The built `coyote` binary, the environment every test hands it, and the
//! dry-run config most binary tests boot it with.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The built binary's path, for sites that need it as a string (an argv
/// vector, a shell line) rather than as a [`Command`].
pub fn path() -> &'static str {
    env!("CARGO_BIN_EXE_coyote")
}

/// The directories a spawned binary resolves its state from. Each one is
/// handed to the child explicitly, so nothing it reads or writes falls back
/// to the developer's own config, cache, home or temp dir.
pub struct Dirs {
    /// `COYOTE_CONFIG_DIR`.
    pub config: PathBuf,
    /// `COYOTE_CACHE_DIR`.
    pub cache: PathBuf,
    /// `HOME`, and `USERPROFILE` for Windows.
    pub home: PathBuf,
    /// `TMPDIR`, and `TMP` and `TEMP` for Windows. A test that looks for what
    /// the child left in its temp dir and has no private one passes
    /// [`env::temp_dir`], the dir the child would have inherited.
    pub temp: PathBuf,
}

/// A [`Command`] for the built binary with its environment set up by
/// [`isolate`].
pub fn coyote_command(dirs: &Dirs) -> Command {
    let mut command = Command::new(path());
    isolate(&mut command, dirs);
    command
}

/// Removes every inherited `COYOTE_*` variable from `command`'s environment,
/// then points it at `dirs`. [`coyote_command`] calls this; a test that
/// launches the binary through a wrapper (`script`, a shell) calls it on the
/// wrapper's command. Anything set on `command` afterwards wins, so a test
/// can still hand the child a `COYOTE_*` variable of its own.
pub fn isolate<'a>(command: &'a mut Command, dirs: &Dirs) -> &'a mut Command {
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            command.env_remove(key);
        }
    }
    command
        .env("COYOTE_CONFIG_DIR", &dirs.config)
        .env("COYOTE_CACHE_DIR", &dirs.cache)
        .env("HOME", &dirs.home)
        .env("USERPROFILE", &dirs.home)
        .env("TMPDIR", &dirs.temp)
        .env("TMP", &dirs.temp)
        .env("TEMP", &dirs.temp)
}

/// A `config.yaml` that selects `dryrun:dry-model`, served by an offline
/// openai client the binary never contacts, with `save: false`.
///
/// Rendered as `model`, `dry_run`, the [`setting`](Self::setting) and
/// [`vault_password_file`](Self::vault_password_file) lines in call order,
/// the `clients` block, `save: false`, then whatever was
/// [`append`](Self::append)ed.
#[derive(Default)]
pub struct DryRunConfig {
    settings: String,
    quoted_api_key: bool,
    tail: String,
}

impl DryRunConfig {
    /// Adds `vault_password_file: '<path>'`. Single-quoted: a double-quoted
    /// scalar would read the backslashes of a Windows path as escapes.
    pub fn vault_password_file(mut self, path: &Path) -> Self {
        self.settings
            .push_str(&format!("vault_password_file: '{}'\n", path.display()));
        self
    }

    /// Adds the top-level `line` ahead of the `clients` block.
    pub fn setting(mut self, line: &str) -> Self {
        self.settings.push_str(line);
        self.settings.push('\n');
        self
    }

    /// Spells the client's api key `'unused'` rather than `unused`.
    pub fn quoted_api_key(mut self) -> Self {
        self.quoted_api_key = true;
        self
    }

    /// Appends `yaml` verbatim after `save: false`.
    pub fn append(mut self, yaml: &str) -> Self {
        self.tail.push_str(yaml);
        self
    }

    pub fn render(&self) -> String {
        let api_key = if self.quoted_api_key {
            "'unused'"
        } else {
            "unused"
        };
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             {settings}\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: dryrun\n\
             \x20   auth: none\n\
             \x20   api_key: {api_key}\n\
             \x20   models:\n\
             \x20     - name: dry-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n\
             {tail}",
            settings = self.settings,
            tail = self.tail,
        )
    }

    /// Writes the rendered config to `<config_dir>/config.yaml`.
    pub fn write(&self, config_dir: &Path) {
        fs::write(config_dir.join("config.yaml"), self.render()).unwrap();
    }
}
