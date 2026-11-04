use crate::dates;
use crate::rules::KINDS;
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub title: Option<String>,
    pub model_events: Option<bool>,
    #[serde(skip_serializing)]
    pub source: Vec<SourceConfig>,
    pub rules: Rules,
    pub repos: BTreeMap<String, RepoRules>,
    pub person: Vec<Person>,
    pub event: Vec<Event>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SourceConfig {
    pub from: String,
    pub group: Option<String>,
    pub exclude: Vec<String>,
    pub submodules: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Rules {
    pub exclude: Vec<String>,
    pub kind_rules: Vec<(String, String)>,
    pub languages: BTreeMap<String, String>,
    pub default_kinds: Option<Vec<String>>,
    pub blame_options: Option<Vec<String>>,
    pub import_subject: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RepoRules {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub import_subject: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Person {
    pub name: String,
    pub emails: Vec<String>,
    pub names: Vec<String>,
    #[serde(rename = "type")]
    pub person_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub date: String,
    pub label: String,
    #[serde(rename = "type", default = "project_event")]
    pub event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

fn project_event() -> String {
    "project".to_string()
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let default = Path::new("planks.toml");
        let path = match path {
            Some(path) => path,
            None if default.exists() => default,
            None => return Ok(Self::default()),
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    /// A config or a manifest can come from anywhere: nothing in it reaches git unchecked.
    pub fn validate(&self) -> Result<()> {
        let allowed = Regex::new(r"^-(w|[MC][0-9]*)$").expect("static pattern");
        for option in self.rules.blame_options.iter().flatten() {
            if !allowed.is_match(option) {
                bail!("blame option `{option}` is not allowed: use -w, -M or -C");
            }
        }
        let known_kind = |kind: &str| KINDS.iter().any(|(key, _)| *key == kind);
        for (kind, _) in &self.rules.kind_rules {
            if !known_kind(kind) {
                bail!("unknown kind `{kind}` in kind_rules");
            }
        }
        for kind in self.rules.default_kinds.iter().flatten() {
            if !known_kind(kind) {
                bail!("unknown kind `{kind}` in default_kinds");
            }
        }
        for person in &self.person {
            let kind = person.person_type.as_deref().unwrap_or("human");
            if !["human", "agent", "bot", "import"].contains(&kind) {
                bail!(
                    "`{}` has the unknown type `{kind}`: use human, agent, bot or import",
                    person.name
                );
            }
        }
        for event in &self.event {
            dates::parse_date(&event.date)?;
        }
        Ok(())
    }

    pub fn blame_options(&self) -> Vec<String> {
        self.rules
            .blame_options
            .clone()
            .unwrap_or_else(|| vec!["-w".to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_keys_instead_of_ignoring_a_typo() {
        assert!(toml::from_str::<Config>("titel = \"x\"").is_err());
    }

    #[test]
    fn refuses_a_blame_option_that_could_write_a_file() {
        let config: Config =
            toml::from_str("[rules]\nblame_options = [\"--output=/tmp/x\"]").unwrap();
        assert!(config.validate().is_err());
        let fine: Config =
            toml::from_str("[rules]\nblame_options = [\"-w\", \"-M\", \"-C30\"]").unwrap();
        assert!(fine.validate().is_ok());
    }

    #[test]
    fn refuses_an_unknown_person_type() {
        let config: Config = toml::from_str("[[person]]\nname = \"A\"\ntype = \"robot\"").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn reads_a_complete_config() {
        let config: Config = toml::from_str(
            r#"
            title = "Acme"
            [[source]]
            from = "github:acme"
            group = "platform"
            exclude = ["*-archive"]
            [rules]
            exclude = ["**/*.sql"]
            [repos.api]
            import_subject = "^Initial import"
            [[person]]
            name = "Ada Lovelace"
            emails = ["ada@acme.dev"]
            [[event]]
            date = "2025-01-01"
            label = "Launch"
            "#,
        )
        .unwrap();
        assert_eq!(config.source[0].group.as_deref(), Some("platform"));
        assert_eq!(config.event[0].event_type, "project");
        assert_eq!(config.blame_options(), ["-w"]);
    }
}
