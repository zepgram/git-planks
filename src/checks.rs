use crate::dates;
use crate::engine::{self, Manifest, first_parent_chain, numstat_between, pick_samples};
use crate::git;
use crate::output;
use crate::rules::PathRules;
use crate::sources;
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

fn counted_lines(
    mirror: &Path,
    commit: &str,
    repo: &str,
    rules: &mut PathRules,
) -> Result<Vec<(u64, String)>> {
    let files = engine::list_files(mirror, commit, repo, rules)?;
    let output = git::read(Some(mirror), &["grep", "-c", "-z", "", commit])?;
    let prefix = format!("{commit}:");
    let mut counted = Vec::new();
    // Records read `path NUL count LF`, and a path may itself hold a newline.
    let mut fields = output.split('\0');
    let mut path = fields.next().unwrap_or_default();
    for field in fields {
        let (count, next) = field.split_once('\n').unwrap_or((field, ""));
        let name = path.strip_prefix(&prefix).unwrap_or(path);
        if files.contains_key(name) {
            counted.push((count.parse().unwrap_or(0), name.to_string()));
        }
        path = next;
    }
    Ok(counted)
}

fn attributed_lines(data: &Value) -> BTreeMap<usize, u64> {
    let mut totals = BTreeMap::new();
    for row in data["series"].as_array().into_iter().flatten() {
        let repo = row[0].as_u64().unwrap_or(0) as usize;
        let last = row[5]
            .as_array()
            .and_then(|points| points.last())
            .and_then(Value::as_u64)
            .unwrap_or(0);
        *totals.entry(repo).or_default() += last;
    }
    totals
}

/// Replays an analysis from its manifest, proves all its files identical, then proves that every
/// line of every counted file has exactly one author. Blame runs again unless `cached` is set.
pub fn verify(directory: &Path, cached: bool) -> Result<()> {
    let manifest = output::read_manifest(directory)?;
    for repo in &manifest.repos {
        sources::ensure_commit(&repo.url, repo.alternate.as_deref(), &repo.tip)?;
    }
    let git_version = git::version()?;
    if git_version != manifest.git {
        println!(
            "note: this report used {}, this machine has {git_version}: blame heuristics can differ.",
            manifest.git
        );
    }
    if manifest.tool_version != env!("CARGO_PKG_VERSION") {
        println!(
            "note: this report used git-planks {}, this is {}: its report template can differ.",
            manifest.tool_version,
            env!("CARGO_PKG_VERSION")
        );
    }
    let outcome = engine::analyze(manifest.clone(), !cached)?;
    let rendered = output::render(&outcome.data)?;
    let mut identical = true;
    for (file, expected) in [
        ("report.html", rendered.report.as_str()),
        ("data.json", rendered.payload.as_str()),
        (
            "identities.json",
            output::identities_text(&outcome.identities)?.as_str(),
        ),
    ] {
        let same =
            std::fs::read_to_string(directory.join(file)).is_ok_and(|found| found == expected);
        identical &= same;
        println!(
            "replay: {file}: {}",
            if same {
                "identical, byte for byte"
            } else {
                "DIFFERENT"
            }
        );
    }
    println!("sha256 {}", output::digest(&rendered.report));

    let mut rules = PathRules::new(&manifest.config)?;
    let attributed = attributed_lines(&outcome.data);
    let mut balanced = true;
    for (index, repo) in outcome.manifest.repos.iter().enumerate() {
        let Some(sampled) = &repo.sampled else {
            continue;
        };
        let mirror = sources::mirror_path(&repo.url);
        let real: u64 = counted_lines(&mirror, sampled, &repo.name, &mut rules)?
            .iter()
            .map(|(lines, _)| *lines)
            .sum();
        let found = attributed.get(&index).copied().unwrap_or(0);
        balanced &= real == found;
        println!(
            "lines: {:<24} attributed {found:>10}  in files {real:>10}  {}",
            output::printable(&repo.name),
            if real == found { "ok" } else { "MISMATCH" }
        );
    }
    if !identical || !balanced {
        bail!("verification failed");
    }
    println!("verified.");
    Ok(())
}

/// Lists what deserves a human look before the figures are trusted: the biggest jumps and largest files.
pub fn audit(directory: &Path, top: usize) -> Result<()> {
    let manifest: Manifest = output::read_manifest(directory)?;
    let data: Value = serde_json::from_str(&std::fs::read_to_string(directory.join("data.json"))?)?;
    let mut rules = PathRules::new(&manifest.config)?;
    let defaults = engine::default_kinds(&manifest.config);
    let kinds: Vec<&str> = data["kinds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|kind| kind["key"].as_str())
        .collect();
    let people: Vec<&str> = data["people"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|person| person["name"].as_str())
        .collect();
    let day_labels: Vec<&str> = data["dates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let days = day_labels
        .iter()
        .map(|label| dates::parse_date(label))
        .collect::<Result<Vec<_>>>()?;

    let mut changes: BTreeMap<(usize, usize, i64), i64> = BTreeMap::new();
    for row in data["series"].as_array().into_iter().flatten() {
        let kind = kinds[row[3].as_u64().unwrap_or(0) as usize];
        if !defaults.iter().any(|default| default == kind) {
            continue;
        }
        let points: Vec<i64> = row[5]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect();
        let mut previous = 0;
        for pair in points.as_chunks::<2>().0 {
            let key = (
                row[0].as_u64().unwrap_or(0) as usize,
                row[1].as_u64().unwrap_or(0) as usize,
                pair[0],
            );
            *changes.entry(key).or_default() += pair[1] - previous;
            previous = pair[1];
        }
    }
    let mut steps: Vec<((usize, usize, i64), i64)> = changes
        .into_iter()
        .filter(|(_, delta)| *delta != 0)
        .collect();
    steps.sort_by(|a, b| b.1.abs().cmp(&a.1.abs()).then(a.0.cmp(&b.0)));

    println!("Biggest jumps. A jump credited to one person is often an import or generated code:");
    println!("exclude its paths in planks.toml, or send its commits to `import_subject`.\n");
    for ((repo_index, person_index, date_index), delta) in steps.into_iter().take(top) {
        let repo = &manifest.repos[repo_index];
        let mirror = sources::mirror_path(&repo.url);
        println!(
            "{:+9}  {}  {}  {}",
            delta,
            day_labels[date_index as usize],
            output::printable(&repo.name),
            output::printable(people[person_index])
        );
        let picks = pick_samples(&first_parent_chain(&mirror, &repo.tip)?, &days);
        let current = picks[date_index as usize].as_deref();
        let earlier = picks[..date_index as usize]
            .iter()
            .rev()
            .flatten()
            .next()
            .map(String::as_str);
        if let Some(current) = current {
            let mut files: Vec<(i64, String)> = numstat_between(&mirror, earlier, current)?
                .into_iter()
                .filter(|(_, path)| rules.classify(&repo.name, path).is_some())
                .collect();
            files.sort_by(|a, b| b.0.abs().cmp(&a.0.abs()).then(a.1.cmp(&b.1)));
            for (lines, path) in files.into_iter().take(3) {
                println!("           {lines:+8} {}", output::printable(&path));
            }
        }
    }

    println!("\nLargest counted files:\n");
    let mut largest: Vec<(u64, String, String)> = Vec::new();
    for repo in &manifest.repos {
        if let Some(sampled) = &repo.sampled {
            let mirror = sources::mirror_path(&repo.url);
            largest.extend(
                counted_lines(&mirror, sampled, &repo.name, &mut rules)?
                    .into_iter()
                    .map(|(lines, path)| (lines, repo.name.clone(), path)),
            );
        }
    }
    largest.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    for (lines, repo, path) in largest.into_iter().take(top) {
        println!(
            "{lines:>9}  {}  {}",
            output::printable(&repo),
            output::printable(&path)
        );
    }
    Ok(())
}
