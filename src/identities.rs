use crate::config::Person;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

pub const IMPORT_PERSON: &str = "Imports (unattributed)";
const GENERIC_EMAILS: &[&str] = &[
    "",
    "noreply@github.com",
    "none@none",
    "you@example.com",
    "root@localhost",
];

const GENERIC_NAMES: &[&str] = &[
    "unknown",
    "root",
    "admin",
    "administrator",
    "user",
    "ubuntu",
    "git",
    "dev",
    "developer",
    "test",
    "none",
    "your name",
    "unknown author",
    "no author",
    "no name",
];

pub type Identity = (String, String);

struct Patterns {
    bot: Regex,
    agent_name: Regex,
    agent_email: Regex,
    noreply: Regex,
}

impl Patterns {
    fn new() -> Self {
        let compile = |pattern: &str| Regex::new(pattern).expect("static pattern");
        Self {
            bot: compile(
                r"(?i)\[bot\]|(^|[\s._-])bot($|[\s._-])|^(renovate|dependabot|greenkeeper|snyk|mergify|semantic-release|github-actions|gitlab-ci|pre-commit-ci|codecov)\b",
            ),
            agent_name: compile(
                r"(?i)^(claude (agent|code)|github copilot|copilot|copilot-swe-agent|cursor ?agent|devin(-ai-integration)?|codex|aider)(\[bot\])?$",
            ),
            agent_email: compile(
                r"(?i)^noreply@anthropic\.com$|copilot@|cursoragent@|^devin-ai-integration",
            ),
            noreply: compile(r"(?i)^(?:\d+\+)?([^@]+)@users\.noreply\.github\.com$"),
        }
    }

    fn automation(&self, name: &str, email: &str) -> Option<&'static str> {
        if self.agent_name.is_match(name.trim()) || self.agent_email.is_match(email) {
            return Some("agent");
        }
        let local = email.split('@').next().unwrap_or("");
        (self.bot.is_match(name) || self.bot.is_match(local)).then_some("bot")
    }
}

pub fn normalized_name(name: &str) -> String {
    let plain: String = name
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A report must not reveal an address because someone set it as their author name.
fn without_address(name: &str) -> String {
    match name.split_once('@') {
        Some((local, _)) if !local.is_empty() => local.to_string(),
        _ => name.to_string(),
    }
}

pub struct Identities {
    canonical: HashMap<Identity, String>,
    types: BTreeMap<String, String>,
    aliases: BTreeMap<String, BTreeSet<Identity>>,
}

impl Identities {
    pub fn new(people: &[Person], seen: &BTreeMap<Identity, u64>) -> Self {
        let mut by_name = HashMap::new();
        let mut by_email = HashMap::new();
        let mut types = BTreeMap::new();
        types.insert(IMPORT_PERSON.to_string(), "import".to_string());
        for person in people {
            types.insert(
                person.name.clone(),
                person
                    .person_type
                    .clone()
                    .unwrap_or_else(|| "human".to_string()),
            );
            by_name.insert(person.name.to_lowercase(), person.name.clone());
            for name in &person.names {
                by_name.insert(name.to_lowercase(), person.name.clone());
            }
            for email in &person.emails {
                by_email.insert(email.to_lowercase(), person.name.clone());
            }
        }
        let patterns = Patterns::new();
        let mut canonical = HashMap::new();
        let mut aliases: BTreeMap<String, BTreeSet<Identity>> = BTreeMap::new();
        let mut automatic: Vec<&Identity> = Vec::new();
        for identity in seen.keys() {
            let (name, email) = identity;
            let explicit = by_name
                .get(&name.to_lowercase())
                .or_else(|| by_email.get(&email.to_lowercase()));
            if let Some(person) = explicit {
                canonical.insert(identity.clone(), person.clone());
                aliases
                    .entry(person.clone())
                    .or_default()
                    .insert(identity.clone());
            } else if let Some(kind) = patterns.automation(name, email) {
                // Automation keeps its own name: it often shares an address with people or other bots.
                let display = without_address(name);
                canonical.insert(identity.clone(), display.clone());
                types
                    .entry(display.clone())
                    .or_insert_with(|| kind.to_string());
                aliases.entry(display).or_default().insert(identity.clone());
            } else {
                automatic.push(identity);
            }
        }
        for members in group_by_shared_keys(&automatic, &patterns) {
            let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
            for identity in &members {
                *counts.entry(identity.0.as_str()).or_default() += seen[*identity];
            }
            let best = counts
                .iter()
                .max_by_key(|(name, count)| (name.trim().contains(' '), **count, **name))
                .map(|(name, _)| without_address(name))
                .unwrap_or_default();
            types
                .entry(best.clone())
                .or_insert_with(|| "human".to_string());
            for identity in members {
                canonical.insert(identity.clone(), best.clone());
                aliases
                    .entry(best.clone())
                    .or_default()
                    .insert(identity.clone());
            }
        }
        Self {
            canonical,
            types,
            aliases,
        }
    }

    pub fn resolve(&self, name: &str, email: &str) -> String {
        self.canonical
            .get(&(name.to_string(), email.to_string()))
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    pub fn type_of(&self, person: &str) -> &str {
        self.types.get(person).map_or("human", String::as_str)
    }

    pub fn report(&self) -> Value {
        Value::Array(
            self.aliases
                .iter()
                .map(|(name, aliases)| {
                    let list: Vec<String> = aliases
                        .iter()
                        .map(|(alias, email)| format!("{alias} <{email}>"))
                        .collect();
                    json!({"name": name, "type": self.type_of(name), "aliases": list})
                })
                .collect(),
        )
    }

    pub fn merged_count(&self) -> usize {
        self.aliases
            .values()
            .filter(|aliases| aliases.len() > 1)
            .count()
    }
}

fn group_by_shared_keys<'a>(
    identities: &[&'a Identity],
    patterns: &Patterns,
) -> Vec<Vec<&'a Identity>> {
    let mut parent: Vec<usize> = (0..identities.len()).collect();
    fn find(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            parent[index] = parent[parent[index]];
            index = parent[index];
        }
        index
    }
    let logins: BTreeSet<String> = identities
        .iter()
        .filter_map(|identity| {
            let lowered = identity.1.to_lowercase();
            let captures = patterns.noreply.captures(&lowered)?;
            Some(captures[1].to_string())
        })
        .collect();
    let mut first_with_key: HashMap<String, usize> = HashMap::new();
    for (index, (name, email)) in identities.iter().map(|i| (&i.0, &i.1)).enumerate() {
        let lowered = email.to_lowercase();
        let mut keys = Vec::new();
        if let Some(noreply) = patterns.noreply.captures(&lowered) {
            keys.push(format!("login:{}", &noreply[1]));
        } else if !GENERIC_EMAILS.contains(&lowered.as_str()) {
            keys.push(format!("email:{lowered}"));
        }
        let normalized = normalized_name(name);
        if GENERIC_NAMES.contains(&normalized.as_str()) {
            // A placeholder says nothing about who wrote the commit.
        } else if normalized.contains(' ') {
            keys.push(format!("name:{normalized}"));
        } else if logins.contains(&normalized) {
            // A lone first name is shared by too many people: it only links to an account of that name.
            keys.push(format!("login:{normalized}"));
        }
        for key in keys {
            match first_with_key.get(&key) {
                Some(other) => {
                    let (a, b) = (find(&mut parent, index), find(&mut parent, *other));
                    parent[a] = b;
                }
                None => {
                    first_with_key.insert(key, index);
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<&Identity>> = BTreeMap::new();
    for (index, identity) in identities.iter().enumerate() {
        let root = find(&mut parent, index);
        groups.entry(root).or_default().push(identity);
    }
    groups.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(entries: &[(&str, &str, u64)]) -> BTreeMap<Identity, u64> {
        entries
            .iter()
            .map(|(name, email, count)| ((name.to_string(), email.to_string()), *count))
            .collect()
    }

    #[test]
    fn merges_the_same_address_under_the_fullest_name() {
        let ids = Identities::new(
            &[],
            &seen(&[("Ada", "ada@x.dev", 50), ("Ada Lovelace", "ada@x.dev", 2)]),
        );
        assert_eq!(ids.resolve("Ada", "ada@x.dev"), "Ada Lovelace");
    }

    #[test]
    fn merges_spellings_of_a_full_name_across_addresses() {
        let ids = Identities::new(
            &[],
            &seen(&[("Noé Martin", "a@x.dev", 3), ("NOE MARTIN", "b@y.dev", 9)]),
        );
        assert_eq!(
            ids.resolve("Noé Martin", "a@x.dev"),
            ids.resolve("NOE MARTIN", "b@y.dev")
        );
    }

    #[test]
    fn links_a_github_noreply_address_to_its_login() {
        let ids = Identities::new(
            &[],
            &seen(&[
                ("adalove", "123+adalove@users.noreply.github.com", 2),
                ("adalove", "ada@x.dev", 2),
            ]),
        );
        assert_eq!(ids.merged_count(), 1);
    }

    #[test]
    fn keeps_two_people_sharing_only_a_first_name_with_different_addresses_when_full_names_differ()
    {
        let ids = Identities::new(
            &[],
            &seen(&[("Alex Martin", "a@x.dev", 1), ("Alex Durand", "b@x.dev", 1)]),
        );
        assert_ne!(
            ids.resolve("Alex Martin", "a@x.dev"),
            ids.resolve("Alex Durand", "b@x.dev")
        );
    }

    #[test]
    fn does_not_merge_two_people_over_a_shared_first_name() {
        let ids = Identities::new(
            &[],
            &seen(&[
                ("Alex", "alex.martin@x.dev", 50),
                ("Alex Martin", "alex.martin@x.dev", 1),
                ("Alex", "alex.durand@y.dev", 50),
                ("Alex Durand", "alex.durand@y.dev", 1),
            ]),
        );
        assert_eq!(ids.resolve("Alex", "alex.martin@x.dev"), "Alex Martin");
        assert_eq!(ids.resolve("Alex", "alex.durand@y.dev"), "Alex Durand");
    }

    #[test]
    fn keeps_an_address_used_as_a_name_out_of_the_display_name() {
        let ids = Identities::new(&[], &seen(&[("bob@corp.example", "bob@corp.example", 1)]));
        assert_eq!(ids.resolve("bob@corp.example", "bob@corp.example"), "bob");
    }

    #[test]
    fn does_not_merge_people_over_a_placeholder_name() {
        let ids = Identities::new(
            &[],
            &seen(&[("Unknown", "a@x.dev", 1), ("unknown", "b@y.dev", 1)]),
        );
        assert_eq!(ids.merged_count(), 0);
    }

    #[test]
    fn ignores_generic_addresses_when_merging() {
        let ids = Identities::new(
            &[],
            &seen(&[
                ("Ada Lovelace", "noreply@github.com", 1),
                ("Alan Turing", "noreply@github.com", 1),
            ]),
        );
        assert_ne!(
            ids.resolve("Ada Lovelace", "noreply@github.com"),
            ids.resolve("Alan Turing", "noreply@github.com")
        );
    }

    #[test]
    fn separates_bots_and_agents_from_people_sharing_their_address() {
        let ids = Identities::new(
            &[],
            &seen(&[
                (
                    "dependabot[bot]",
                    "49699333+dependabot[bot]@users.noreply.github.com",
                    4,
                ),
                ("Company Bot", "web@acme.dev", 2),
                ("Claude Agent", "web@acme.dev", 2),
                ("Claude Monet", "claude@acme.dev", 2),
            ]),
        );
        assert_eq!(ids.type_of("dependabot[bot]"), "bot");
        assert_eq!(ids.type_of("Company Bot"), "bot");
        assert_eq!(ids.type_of("Claude Agent"), "agent");
        assert_eq!(ids.type_of("Claude Monet"), "human");
    }

    #[test]
    fn explicit_people_win_over_automatic_rules() {
        let people = vec![Person {
            name: "Release Train".to_string(),
            emails: vec!["web@acme.dev".to_string()],
            names: vec![],
            person_type: Some("bot".to_string()),
        }];
        let ids = Identities::new(&people, &seen(&[("Someone", "web@acme.dev", 1)]));
        assert_eq!(ids.resolve("Someone", "web@acme.dev"), "Release Train");
        assert_eq!(ids.type_of("Release Train"), "bot");
    }
}
