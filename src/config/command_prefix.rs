use crate::repl::builtin_command_names;

use anyhow::bail;
use serde::Deserialize;
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

static BUILTIN_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(builtin_command_names);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandPrefix {
    #[default]
    #[serde(alias = ".")]
    Dot,
    #[serde(alias = "/")]
    Slash,
    #[serde(alias = "either")]
    Both,
}

impl CommandPrefix {
    pub const NAMES: [&'static str; 3] = ["dot", "slash", "both"];

    pub fn accepts(self, c: char) -> bool {
        match self {
            Self::Dot => c == '.',
            Self::Slash => c == '/',
            Self::Both => c == '.' || c == '/',
        }
    }

    pub fn display(self) -> char {
        match self {
            Self::Dot | Self::Both => '.',
            Self::Slash => '/',
        }
    }

    pub fn with_dot(self) -> Self {
        match self {
            Self::Dot => Self::Dot,
            Self::Slash | Self::Both => Self::Both,
        }
    }

    pub fn normalize(self, cmd: &str) -> Option<Cow<'_, str>> {
        let first = cmd.chars().next()?;
        if !self.accepts(first) {
            return None;
        }
        if first == '.' {
            Some(Cow::Borrowed(cmd))
        } else {
            Some(Cow::Owned(format!(".{}", &cmd[first.len_utf8()..])))
        }
    }

    pub fn render_hints(self, text: &str) -> Cow<'_, str> {
        if self.display() == '.' {
            return Cow::Borrowed(text);
        }
        let mut out = String::new();
        let mut last = 0;
        for (i, _) in text.match_indices('.') {
            let at_boundary = text[..i]
                .chars()
                .next_back()
                .is_none_or(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '('));
            if !at_boundary || !is_hint_name(&text[i + 1..]) {
                continue;
            }
            out.push_str(&text[last..i]);
            out.push(self.display());
            last = i + 1;
        }
        if out.is_empty() {
            return Cow::Borrowed(text);
        }
        out.push_str(&text[last..]);
        Cow::Owned(out)
    }
}

fn is_hint_name(rest: &str) -> bool {
    if rest.starts_with("<name>") {
        return true;
    }
    let len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    len > 0 && BUILTIN_NAMES.contains(&&rest[..len])
}

impl FromStr for CommandPrefix {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "dot" | "." => Ok(Self::Dot),
            "slash" | "/" => Ok(Self::Slash),
            "both" | "either" => Ok(Self::Both),
            _ => bail!(
                "Invalid value '{value}', expected one of: {}",
                Self::NAMES.join(", ")
            ),
        }
    }
}

impl fmt::Display for CommandPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dot => "dot",
            Self::Slash => "slash",
            Self::Both => "both",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Debug)]
    struct Holder {
        command_prefix: CommandPrefix,
    }

    fn parse(yaml: &str) -> CommandPrefix {
        serde_yaml::from_str::<Holder>(yaml).unwrap().command_prefix
    }

    #[test]
    fn deserializes_lowercase_names_and_aliases() {
        assert_eq!(parse("command_prefix: dot"), CommandPrefix::Dot);
        assert_eq!(parse("command_prefix: slash"), CommandPrefix::Slash);
        assert_eq!(parse("command_prefix: both"), CommandPrefix::Both);
        assert_eq!(parse("command_prefix: '.'"), CommandPrefix::Dot);
        assert_eq!(parse("command_prefix: '/'"), CommandPrefix::Slash);
        assert_eq!(parse("command_prefix: either"), CommandPrefix::Both);
    }

    #[test]
    fn bogus_value_is_a_config_error_naming_the_variants() {
        let err = serde_yaml::from_str::<Holder>("command_prefix: bogus")
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("dot") && err.contains("slash") && err.contains("both"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn default_is_dot() {
        assert_eq!(CommandPrefix::default(), CommandPrefix::Dot);
    }

    #[test]
    fn from_str_accepts_names_and_aliases_and_rejects_bogus() {
        assert_eq!("dot".parse::<CommandPrefix>().unwrap(), CommandPrefix::Dot);
        assert_eq!("/".parse::<CommandPrefix>().unwrap(), CommandPrefix::Slash);
        assert_eq!(
            "either".parse::<CommandPrefix>().unwrap(),
            CommandPrefix::Both
        );
        let err = "bogus".parse::<CommandPrefix>().unwrap_err().to_string();
        assert!(err.contains("dot, slash, both"), "unexpected error: {err}");
    }

    #[test]
    fn normalize_rewrites_accepted_sigils_to_dot() {
        assert_eq!(
            CommandPrefix::Dot.normalize(".info").as_deref(),
            Some(".info")
        );
        assert_eq!(CommandPrefix::Dot.normalize("/info"), None);
        assert_eq!(
            CommandPrefix::Slash.normalize("/info").as_deref(),
            Some(".info")
        );
        assert_eq!(CommandPrefix::Slash.normalize(".info"), None);
        assert_eq!(
            CommandPrefix::Both.normalize("/info").as_deref(),
            Some(".info")
        );
        assert_eq!(
            CommandPrefix::Both.normalize(".info").as_deref(),
            Some(".info")
        );
        assert_eq!(CommandPrefix::Both.normalize("info"), None);
        assert_eq!(CommandPrefix::Both.normalize(""), None);
    }

    #[test]
    fn display_sigil_is_slash_only_for_slash() {
        assert_eq!(CommandPrefix::Dot.display(), '.');
        assert_eq!(CommandPrefix::Both.display(), '.');
        assert_eq!(CommandPrefix::Slash.display(), '/');
    }

    #[test]
    fn with_dot_only_widens_slash() {
        assert_eq!(CommandPrefix::Dot.with_dot(), CommandPrefix::Dot);
        assert_eq!(CommandPrefix::Slash.with_dot(), CommandPrefix::Both);
        assert_eq!(CommandPrefix::Both.with_dot(), CommandPrefix::Both);
    }

    #[test]
    fn render_hints_is_identity_for_dot_and_both() {
        let text = "Usage: .set <key> <value>";
        assert_eq!(CommandPrefix::Dot.render_hints(text), text);
        assert_eq!(CommandPrefix::Both.render_hints(text), text);
    }

    #[test]
    fn render_hints_rewrites_builtin_names_at_token_boundaries() {
        let slash = CommandPrefix::Slash;
        assert_eq!(
            slash.render_hints("Usage: .set <key> <value>"),
            "Usage: /set <key> <value>"
        );
        assert_eq!(
            slash.render_hints(r#"Type ".help" for help"#),
            r#"Type "/help" for help"#
        );
        assert_eq!(
            slash.render_hints("runs as .<name> [args]"),
            "runs as /<name> [args]"
        );
        assert_eq!(slash.render_hints(".info"), "/info");
        assert_eq!(
            slash.render_hints("(see `.install --help`)"),
            "(see `/install --help`)"
        );
        assert_eq!(slash.render_hints("'.empty session'"), "'/empty session'");
        assert_eq!(
            slash.render_hints(".exit rag\n.exit role"),
            "/exit rag\n/exit role"
        );
    }

    #[test]
    fn render_hints_leaves_non_command_dots_alone() {
        let slash = CommandPrefix::Slash;
        for text in [
            "saved to messages.md",
            "edit config.yaml",
            "foo.info",
            "mcp_tools.<server>",
            "under .coyote/",
            "./file1.txt",
            "a .yaml file",
        ] {
            assert_eq!(slash.render_hints(text), text, "{text}");
        }
    }
}
