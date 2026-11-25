use anyhow::bail;
use serde::Deserialize;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum UiMode {
    Repl,
    Tui,
}

impl UiMode {
    pub const NAMES: [&'static str; 2] = ["repl", "tui"];
}

/// The shell an interactive run starts: `--tui` / `--repl`, then the `ui`
/// key, then the TUI when both stdin and stdout are terminals and the REPL
/// otherwise. An explicit choice is kept even without a terminal, so the
/// TUI's own "No TTY" refusal still answers an explicit `tui`.
pub(crate) fn interactive_ui(
    stdin_tty: bool,
    stdout_tty: bool,
    cli_tui: bool,
    cli_repl: bool,
    config: Option<UiMode>,
) -> UiMode {
    if cli_tui {
        UiMode::Tui
    } else if cli_repl {
        UiMode::Repl
    } else if let Some(mode) = config {
        mode
    } else if stdin_tty && stdout_tty {
        UiMode::Tui
    } else {
        UiMode::Repl
    }
}

impl FromStr for UiMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "repl" => Ok(Self::Repl),
            "tui" => Ok(Self::Tui),
            _ => bail!(
                "Invalid value '{value}', expected one of: {}",
                Self::NAMES.join(", ")
            ),
        }
    }
}

impl TryFrom<String> for UiMode {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl fmt::Display for UiMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Repl => "repl",
            Self::Tui => "tui",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Debug)]
    struct Holder {
        #[serde(default)]
        ui: Option<UiMode>,
    }

    fn parse(yaml: &str) -> UiMode {
        serde_yaml::from_str::<Holder>(yaml).unwrap().ui.unwrap()
    }

    #[test]
    fn deserializes_lowercase_names() {
        assert_eq!(parse("ui: repl"), UiMode::Repl);
        assert_eq!(parse("ui: tui"), UiMode::Tui);
    }

    #[test]
    fn bogus_value_is_a_config_error_naming_the_variants() {
        let err = serde_yaml::from_str::<Holder>("ui: bogus")
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("Invalid value 'bogus', expected one of: repl, tui"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_missing_or_null_ui_is_unset() {
        for yaml in ["{}", "ui: null"] {
            assert_eq!(
                serde_yaml::from_str::<Holder>(yaml).unwrap().ui,
                None,
                "{yaml}"
            );
        }
    }

    #[test]
    fn interactive_ui_flags_beat_the_config_and_the_terminal() {
        for (stdin_tty, stdout_tty) in [(true, true), (true, false), (false, true), (false, false)]
        {
            for config in [None, Some(UiMode::Repl), Some(UiMode::Tui)] {
                let pick = |tui, repl| interactive_ui(stdin_tty, stdout_tty, tui, repl, config);
                assert_eq!(
                    pick(true, false),
                    UiMode::Tui,
                    "{stdin_tty} {stdout_tty} {config:?}"
                );
                assert_eq!(
                    pick(false, true),
                    UiMode::Repl,
                    "{stdin_tty} {stdout_tty} {config:?}"
                );
            }
        }
    }

    /// An explicit `ui` is kept without a terminal, so `main` still reaches
    /// the TUI's "No TTY for TUI" refusal for `ui: tui`.
    #[test]
    fn interactive_ui_config_beats_the_terminal_default() {
        for (stdin_tty, stdout_tty) in [(true, true), (true, false), (false, true), (false, false)]
        {
            for mode in [UiMode::Repl, UiMode::Tui] {
                assert_eq!(
                    interactive_ui(stdin_tty, stdout_tty, false, false, Some(mode)),
                    mode,
                    "{stdin_tty} {stdout_tty}"
                );
            }
        }
    }

    #[test]
    fn interactive_ui_default_is_the_tui_only_with_both_terminals() {
        let default =
            |stdin_tty, stdout_tty| interactive_ui(stdin_tty, stdout_tty, false, false, None);
        assert_eq!(default(true, true), UiMode::Tui);
        assert_eq!(default(true, false), UiMode::Repl);
        assert_eq!(default(false, true), UiMode::Repl);
        assert_eq!(default(false, false), UiMode::Repl);
    }

    #[test]
    fn from_str_accepts_names_and_rejects_bogus() {
        assert_eq!("repl".parse::<UiMode>().unwrap(), UiMode::Repl);
        assert_eq!("tui".parse::<UiMode>().unwrap(), UiMode::Tui);
        let err = "bogus".parse::<UiMode>().unwrap_err().to_string();
        assert_eq!(err, "Invalid value 'bogus', expected one of: repl, tui");
    }

    #[test]
    fn display_round_trips_through_from_str() {
        for mode in [UiMode::Repl, UiMode::Tui] {
            assert_eq!(mode.to_string().parse::<UiMode>().unwrap(), mode);
        }
        for name in UiMode::NAMES {
            assert_eq!(name.parse::<UiMode>().unwrap().to_string(), name);
        }
    }
}
