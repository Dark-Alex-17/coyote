use anyhow::bail;
use serde::Deserialize;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum UiMode {
    #[default]
    Repl,
    Tui,
}

impl UiMode {
    pub const NAMES: [&'static str; 2] = ["repl", "tui"];
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
        ui: UiMode,
    }

    fn parse(yaml: &str) -> UiMode {
        serde_yaml::from_str::<Holder>(yaml).unwrap().ui
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
    fn default_is_repl() {
        assert_eq!(UiMode::default(), UiMode::Repl);
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

    /// The value a parent exports to a nested coyote as `COYOTE_UI` is the
    /// value the child parses back: the two shell kinds round-trip through
    /// `UiMode` unchanged.
    #[test]
    fn the_exported_shell_kinds_parse_back_unchanged() {
        use crate::config::UiKind;
        for kind in [UiKind::Repl, UiKind::Tui] {
            let name = kind.as_str();
            assert_eq!(name.parse::<UiMode>().unwrap().to_string(), name);
        }
    }
}
