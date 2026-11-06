use crate::config::MODEL_VENDORS;
use crate::{dates, sources};
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const CATALOG: &str = "https://models.dev/api.json";
const VARIANT: &str = r"(?i)\b(mini|nano|lite|embed\w*|tts|image|audio|realtime|search|transcribe|moderation|ocr|vision|live|instruct|small|tiny|turbo|chat|gemma|omni|spark|highspeed|flashx|it|custom tools|\d+b)\b";

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Release {
    pub date: String,
    pub label: String,
    pub vendor: String,
}

/// The first one is the provider key in the catalog, the others the owners it credits in
/// `canonical_model_id`, which tells a lab's own model from one it resells.
fn owners(vendor: &'static str) -> Vec<&'static str> {
    match vendor {
        "moonshot" => vec!["moonshotai"],
        "zai" => vec!["zai", "zhipuai"],
        _ => vec![vendor],
    }
}

fn key(label: &str) -> String {
    label
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
        .collect()
}

pub fn new_releases(catalog: &Value, known: &[Release], today: &str) -> Vec<Release> {
    let variant = Regex::new(VARIANT).expect("valid pattern");
    let snapshot = Regex::new(r"\d{4}-?\d{2}-?\d{2}|-\d{4}$").expect("valid pattern");
    let decoration = Regex::new(r"(?i)\s*\(.*?\)|\s+preview\b").expect("valid pattern");
    let mut found = Vec::new();
    for vendor in MODEL_VENDORS {
        let owners = owners(vendor);
        let mine = known.iter().filter(|release| release.vendor == vendor);
        let last = mine.clone().map(|release| release.date.as_str()).max();
        let labels: BTreeSet<String> = mine.map(|release| key(&release.label)).collect();
        let mut by_day: BTreeMap<&str, String> = BTreeMap::new();
        let models = catalog[owners[0]]["models"]
            .as_object()
            .into_iter()
            .flatten();
        for (id, model) in models {
            let date = model["release_date"].as_str().unwrap_or("");
            let owner = model["canonical_model_id"]
                .as_str()
                .and_then(|canonical| canonical.split('/').next())
                .unwrap_or(owners[0]);
            if date.len() != 10
                || Some(date) <= last
                || date > today
                || model["tool_call"] != true
                || !owners.contains(&owner)
                || snapshot.is_match(id)
            {
                continue;
            }
            let name = decoration.replace_all(model["name"].as_str().unwrap_or(""), "");
            let name = name.trim();
            let label = name.strip_prefix("Claude ").unwrap_or(name);
            if label.is_empty() || variant.is_match(label) || labels.contains(&key(label)) {
                continue;
            }
            by_day
                .entry(date)
                .and_modify(|kept| {
                    if (label.len(), label) < (kept.len(), kept.as_str()) {
                        *kept = label.to_string();
                    }
                })
                .or_insert_with(|| label.to_string());
        }
        found.extend(by_day.into_iter().map(|(date, label)| Release {
            date: date.to_string(),
            label,
            vendor: vendor.to_string(),
        }));
    }
    found
}

pub fn render(releases: &[Release]) -> String {
    let lines: Vec<String> = releases
        .iter()
        .map(|release| {
            format!(
                "  {{ \"date\": {}, \"label\": {}, \"type\": \"model\", \"vendor\": {} }}",
                json!(release.date),
                json!(release.label),
                json!(release.vendor)
            )
        })
        .collect();
    format!("[\n{}\n]\n", lines.join(",\n"))
}

pub fn update(file: &Path) -> Result<()> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    let mut releases: Vec<Release> = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a list of model releases", file.display()))?;
    let (status, catalog) = sources::get_json(&sources::agent(), CATALOG, &[])?;
    if status != 200 || !catalog.is_object() {
        bail!("{CATALOG} answered {status}");
    }
    let added = new_releases(&catalog, &releases, &dates::format_date(dates::today()));
    for release in &added {
        println!("{}  {:<10} {}", release.date, release.vendor, release.label);
    }
    releases.extend(added);
    releases.sort_by(|a, b| (&a.date, &a.vendor, &a.label).cmp(&(&b.date, &b.vendor, &b.label)));
    std::fs::write(file, render(&releases))
        .with_context(|| format!("cannot write {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(date: &str, label: &str, vendor: &str) -> Release {
        Release {
            date: date.into(),
            label: label.into(),
            vendor: vendor.into(),
        }
    }

    #[test]
    fn the_shipped_list_is_written_as_it_is_read() {
        let text = include_str!("../assets/model-events.json");
        let releases: Vec<Release> = serde_json::from_str(text).unwrap();
        assert_eq!(render(&releases), text);
    }

    #[test]
    fn keeps_one_new_flagship_per_lab_and_day() {
        let catalog = json!({
            "openai": {"models": {
                "gpt-7": {"name": "GPT-7", "release_date": "2026-10-01", "tool_call": true},
                "gpt-7-pro": {"name": "GPT-7 Pro", "release_date": "2026-10-01", "tool_call": true},
                "gpt-7-mini": {"name": "GPT-7 mini", "release_date": "2026-10-02", "tool_call": true},
                "gpt-7-2026-10-01": {"name": "GPT-7", "release_date": "2026-10-03", "tool_call": true},
                "gpt-6": {"name": "GPT-6", "release_date": "2026-09-30", "tool_call": true},
                "gpt-8": {"name": "GPT-8", "release_date": "2027-01-01", "tool_call": true},
                "whisper": {"name": "Whisper", "release_date": "2026-10-04", "tool_call": false}
            }},
            "anthropic": {"models": {
                "claude-opus-6": {"name": "Claude Opus 6 (latest)", "release_date": "2026-10-05", "tool_call": true},
                "claude-opus-5": {"name": "Claude Opus 5", "release_date": "2026-10-06", "tool_call": true}
            }},
            "alibaba": {"models": {
                "kimi-k4": {"name": "Kimi K4", "release_date": "2026-10-07", "tool_call": true, "canonical_model_id": "moonshotai/kimi-k4"}
            }},
            "zai": {"models": {
                "glm-6": {"name": "GLM-6 Preview", "release_date": "2026-10-08", "tool_call": true, "canonical_model_id": "zhipuai/glm-6"}
            }}
        });
        let known = [
            release("2026-09-30", "GPT-6", "openai"),
            release("2026-09-01", "Opus 5", "anthropic"),
        ];
        assert_eq!(
            new_releases(&catalog, &known, "2026-11-01"),
            [
                release("2026-10-05", "Opus 6", "anthropic"),
                release("2026-10-01", "GPT-7", "openai"),
                release("2026-10-08", "GLM-6", "zai"),
            ]
        );
    }
}
