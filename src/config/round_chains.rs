use std::fmt;
use std::str::FromStr;

use anyhow::bail;
use serde::Deserialize;

/// What `memory__write` of `<base>-r<N>` does to the older `<base>-r<M>` files
/// in the same scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoundChains {
    #[default]
    Supersede,
    Keep,
}

impl RoundChains {
    pub const NAMES: [&'static str; 2] = ["supersede", "keep"];
}

impl FromStr for RoundChains {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "supersede" => Ok(Self::Supersede),
            "keep" => Ok(Self::Keep),
            _ => bail!(
                "Invalid value '{value}', expected one of: {}",
                Self::NAMES.join(", ")
            ),
        }
    }
}

impl fmt::Display for RoundChains {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Supersede => "supersede",
            Self::Keep => "keep",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Debug)]
    struct Holder {
        memory_round_chains: RoundChains,
    }

    fn parse(yaml: &str) -> RoundChains {
        serde_yaml::from_str::<Holder>(yaml)
            .unwrap()
            .memory_round_chains
    }

    #[test]
    fn deserializes_kebab_case_names() {
        assert_eq!(
            parse("memory_round_chains: supersede"),
            RoundChains::Supersede
        );
        assert_eq!(parse("memory_round_chains: keep"), RoundChains::Keep);
    }

    #[test]
    fn bogus_value_is_a_config_error_naming_the_variants() {
        let err = serde_yaml::from_str::<Holder>("memory_round_chains: bogus")
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("supersede") && err.contains("keep"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn default_is_supersede() {
        assert_eq!(RoundChains::default(), RoundChains::Supersede);
    }

    #[test]
    fn from_str_accepts_names_and_rejects_bogus_with_a_teaching_error() {
        assert_eq!(
            "supersede".parse::<RoundChains>().unwrap(),
            RoundChains::Supersede
        );
        assert_eq!("keep".parse::<RoundChains>().unwrap(), RoundChains::Keep);
        let err = "bogus".parse::<RoundChains>().unwrap_err().to_string();
        assert_eq!(
            err,
            "Invalid value 'bogus', expected one of: supersede, keep"
        );
        assert_eq!(RoundChains::Supersede.to_string(), "supersede");
        assert_eq!(RoundChains::Keep.to_string(), "keep");
    }
}
