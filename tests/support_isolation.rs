//! Pins the environment `support::bin::coyote_command` hands the child: the
//! dir variables come from `Dirs`, inherited `COYOTE_*` variables are removed,
//! and anything set on the command afterwards wins.
//!
//! This crate holds a single test because it mutates the process environment.

mod support;

use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use support::bin::{Dirs, PINNED_VARS, coyote_command};

const PROBE: &str = "COYOTE_ISOLATION_PROBE";

#[test]
fn coyote_command_pins_the_dirs_clears_inherited_coyote_vars_and_lets_later_vars_win() {
    // SAFETY: this is the only test in this crate, so no other thread reads
    // the environment while it is mutated.
    unsafe { env::set_var(PROBE, "inherited") };

    let base = env::temp_dir().join("coyote-support-isolation");
    let dirs = Dirs {
        config: base.join("config"),
        cache: base.join("cache"),
        home: base.join("home"),
        temp: base.join("temp"),
    };
    let mut command = coyote_command(&dirs);
    command.env("COYOTE_LATER", "1");
    let envs: HashMap<OsString, Option<OsString>> = command
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
        .collect();

    let expected: [(&str, &PathBuf); 7] = [
        ("COYOTE_CONFIG_DIR", &dirs.config),
        ("COYOTE_CACHE_DIR", &dirs.cache),
        ("HOME", &dirs.home),
        ("USERPROFILE", &dirs.home),
        ("TMPDIR", &dirs.temp),
        ("TMP", &dirs.temp),
        ("TEMP", &dirs.temp),
    ];
    for (key, dir) in expected {
        assert_eq!(
            envs.get(OsStr::new(key)),
            Some(&Some(dir.as_os_str().to_os_string())),
            "coyote_command must set {key} from Dirs"
        );
    }
    let mut pinned: Vec<&str> = expected.iter().map(|(key, _)| *key).collect();
    let mut listed = PINNED_VARS.to_vec();
    pinned.sort_unstable();
    listed.sort_unstable();
    assert_eq!(
        pinned, listed,
        "PINNED_VARS must name exactly the vars isolate sets"
    );
    assert_eq!(
        envs.get(OsStr::new(PROBE)),
        Some(&None),
        "coyote_command must remove an inherited COYOTE_* variable"
    );
    assert_eq!(
        envs.get(OsStr::new("COYOTE_LATER")),
        Some(&Some(OsString::from("1"))),
        "a variable set after coyote_command must reach the child"
    );

    // SAFETY: as above, no other thread reads the environment.
    unsafe { env::remove_var(PROBE) };
}
