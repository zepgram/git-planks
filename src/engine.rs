use crate::config::Config;
use crate::dates;
use crate::git;
use crate::identities::{IMPORT_PERSON, Identities, Identity};
use crate::rules::{DEFAULT_KINDS, KINDS, PathRules};
use crate::sources;
use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};

const MODEL_EVENTS: &str = include_str!("../assets/model-events.json");
const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRepo {
    pub name: String,
    pub group: String,
    pub url: String,
    pub branch: String,
    pub tip: String,
    pub sampled: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alternate: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub name: String,
    pub url: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub tool_version: String,
    pub git: String,
    pub until: String,
    pub interval_weeks: i64,
    pub config: Config,
    pub repos: Vec<ManifestRepo>,
    pub skipped: Vec<Skipped>,
}

fn is_object_id(text: &str) -> bool {
    (text.len() == 40 || text.len() == 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Manifest {
    /// A manifest can be handed over by someone else: nothing in it reaches git unchecked.
    pub fn validate(&self) -> Result<()> {
        self.config.validate()?;
        dates::parse_date(&self.until)?;
        if self.interval_weeks < 1 {
            bail!("interval_weeks must be at least 1");
        }
        for repo in &self.repos {
            if !is_object_id(&repo.tip) || !repo.sampled.as_deref().is_none_or(is_object_id) {
                bail!("`{}` does not point at a commit id", repo.name);
            }
            let usable = |url: &str| sources::is_remote_url(url) || Path::new(url).is_absolute();
            if !usable(&repo.url) || !repo.alternate.as_deref().is_none_or(sources::is_remote_url) {
                bail!("`{}` has an unusable address", repo.name);
            }
        }
        Ok(())
    }
}

struct Commit {
    name: String,
    email: String,
    time: i64,
    subject: String,
}

type CubeKey = (String, String, String, String);
type Blame = BTreeMap<String, u64>;

pub fn kind_bit(kind: &str) -> usize {
    KINDS.iter().position(|(key, _)| *key == kind).unwrap_or(0)
}

pub fn default_kinds(config: &Config) -> Vec<String> {
    config
        .rules
        .default_kinds
        .clone()
        .unwrap_or_else(|| DEFAULT_KINDS.iter().map(ToString::to_string).collect())
}

fn read_commits(mirror: &Path, tip: &str) -> Result<HashMap<String, Commit>> {
    let mailmap = format!("mailmap.blob={tip}:.mailmap");
    let output = git::read(
        Some(mirror),
        &[
            "-c",
            &mailmap,
            "log",
            "--format=%H%x00%aN%x00%aE%x00%at%x00%s",
            tip,
        ],
    )?;
    let mut commits = HashMap::new();
    for line in output.split('\n').filter(|line| !line.is_empty()) {
        let fields: Vec<&str> = line.splitn(5, '\0').collect();
        if let [sha, name, email, time, subject] = fields[..] {
            commits.insert(
                sha.to_string(),
                Commit {
                    name: name.to_string(),
                    email: email.to_string(),
                    time: time.parse().unwrap_or(0),
                    subject: subject.to_string(),
                },
            );
        }
    }
    Ok(commits)
}

pub fn first_parent_chain(mirror: &Path, tip: &str) -> Result<Vec<(String, i64)>> {
    let output = git::read(
        Some(mirror),
        &["log", "--first-parent", "--format=%H %ct", tip],
    )?;
    Ok(output
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(sha, time)| (sha.to_string(), time.parse().unwrap_or(0)))
        .collect())
}

/// Picks, for each date, the newest first-parent commit made by then. Walking from the tip tolerates
/// committer dates that go backwards, which rebases and imports produce.
pub fn pick_samples(chain: &[(String, i64)], dates: &[i64]) -> Vec<Option<String>> {
    let mut floor = chain.len();
    let mut picks = Vec::with_capacity(dates.len());
    for (index, date) in dates.iter().enumerate() {
        let limit = if index + 1 == dates.len() {
            dates::end_of_day(*date)
        } else {
            dates::start_of_day(*date)
        };
        match chain[..floor].iter().position(|(_, time)| *time <= limit) {
            Some(found) => {
                floor = found + 1;
                picks.push(Some(chain[found].0.clone()));
            }
            None => picks.push(None),
        }
    }
    picks
}

fn parse_blame(output: &str) -> Blame {
    let mut counts = Blame::new();
    for line in output.split('\n') {
        let mut tokens = line.split(' ');
        if let (Some(sha), Some(_), Some(_), Some(lines), None) = (
            tokens.next(),
            tokens.next(),
            tokens.next(),
            tokens.next(),
            tokens.next(),
        ) && (sha.len() == 40 || sha.len() == 64)
            && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            && let Ok(lines) = lines.parse::<u64>()
        {
            *counts.entry(sha.to_string()).or_default() += lines;
        }
    }
    counts
}

fn blame(mirror: &Path, commit: &str, path: &str, options: &[String]) -> Result<Blame> {
    let mut args: Vec<&str> = vec!["blame", "--incremental"];
    args.extend(options.iter().map(String::as_str));
    args.extend([commit, "--", path]);
    Ok(parse_blame(&git::read(Some(mirror), &args)?))
}

pub fn list_files(
    mirror: &Path,
    commit: &str,
    repo: &str,
    rules: &mut PathRules,
) -> Result<BTreeMap<String, String>> {
    let output = git::read(Some(mirror), &["ls-tree", "-r", "-z", commit])?;
    let mut files = BTreeMap::new();
    for entry in output.split('\0') {
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let fields: Vec<&str> = meta.split(' ').collect();
        // A path that is not valid UTF-8 cannot be handed back to git once decoded.
        if let [mode, "blob", blob] = fields[..]
            && mode != "120000"
            && !path.contains('\u{FFFD}')
            && rules.classify(repo, path).is_some()
        {
            files.insert(path.to_string(), blob.to_string());
        }
    }
    Ok(files)
}

struct BlameCache {
    entries: HashMap<(String, String), Blame>,
    writer: Option<BufWriter<File>>,
}

impl BlameCache {
    /// Entries live per git version, as blame heuristics change between versions. `fresh` ignores
    /// them and writes none: what it proves does not rest on an earlier run.
    fn open(mirror: &Path, options: &[String], fresh: bool) -> Result<Self> {
        if fresh {
            return Ok(Self {
                entries: HashMap::new(),
                writer: None,
            });
        }
        let digest =
            Sha256::digest(format!("{} {}", git::version()?, options.join(" ")).as_bytes());
        let hash: String = digest
            .iter()
            .take(4)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let directory = sources::cache_dir().join(format!("blame-{hash}"));
        fs::create_dir_all(&directory)?;
        let name = mirror
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path: PathBuf = directory.join(format!("{name}.jsonl"));
        let mut entries = HashMap::new();
        if let Ok(file) = File::open(&path) {
            for line in BufReader::new(file)
                .lines()
                .map_while(std::result::Result::ok)
            {
                // A line cut by an interrupted run is only a cache miss.
                if let Ok((commit, file_path, hist)) =
                    serde_json::from_str::<(String, String, Blame)>(&line)
                {
                    entries.insert((commit, file_path), hist);
                }
            }
        }
        let writer = BufWriter::new(OpenOptions::new().create(true).append(true).open(&path)?);
        Ok(Self {
            entries,
            writer: Some(writer),
        })
    }

    fn store(&mut self, commit: &str, path: &str, hist: Blame) -> Result<()> {
        if let Some(writer) = &mut self.writer {
            writeln!(writer, "{}", serde_json::to_string(&(commit, path, &hist))?)?;
        }
        self.entries
            .insert((commit.to_string(), path.to_string()), hist);
        Ok(())
    }
}

struct Attribution<'a> {
    commits: &'a HashMap<String, Commit>,
    identities: &'a Identities,
    import_subject: Option<Regex>,
    memo: HashMap<String, (String, String)>,
}

impl Attribution<'_> {
    fn of(&mut self, sha: &str) -> Result<(String, String)> {
        if let Some(found) = self.memo.get(sha) {
            return Ok(found.clone());
        }
        let commit = self
            .commits
            .get(sha)
            .ok_or_else(|| anyhow!("unknown commit {sha}"))?;
        let imported = self
            .import_subject
            .as_ref()
            .is_some_and(|rule| rule.is_match(&commit.subject));
        let person = if imported {
            IMPORT_PERSON.to_string()
        } else {
            self.identities.resolve(&commit.name, &commit.email)
        };
        let found = (person, dates::quarter_of(commit.time));
        self.memo.insert(sha.to_string(), found.clone());
        Ok(found)
    }
}

struct RepoRun<'a> {
    name: &'a str,
    mirror: &'a Path,
    options: &'a [String],
    fresh: bool,
}

fn walk(
    run: &RepoRun,
    picks: &[Option<String>],
    rules: &mut PathRules,
    attribution: &mut Attribution,
) -> Result<BTreeMap<CubeKey, Vec<i64>>> {
    let mut cache = BlameCache::open(run.mirror, run.options, run.fresh)?;
    let mut state: HashMap<String, (String, BTreeMap<CubeKey, i64>)> = HashMap::new();
    let mut totals: BTreeMap<CubeKey, i64> = BTreeMap::new();
    let mut series: BTreeMap<CubeKey, Vec<i64>> = BTreeMap::new();
    let mut previous: Option<&str> = None;
    let interactive = std::io::stderr().is_terminal();
    let mut blamed = 0;
    for (index, pick) in picks.iter().enumerate() {
        let Some(commit) = pick.as_deref() else {
            continue;
        };
        if previous == Some(commit) {
            continue;
        }
        previous = Some(commit);
        let files = list_files(run.mirror, commit, run.name, rules)?;
        let changed: Vec<&String> = files
            .iter()
            .filter(|(path, blob)| state.get(*path).is_none_or(|(known, _)| known != *blob))
            .map(|(path, _)| path)
            .collect();
        let missing: Vec<&String> = changed
            .iter()
            .filter(|path| {
                !cache
                    .entries
                    .contains_key(&(commit.to_string(), (**path).clone()))
            })
            .copied()
            .collect();
        let results: Vec<Result<Blame>> = missing
            .par_iter()
            .map(|path| blame(run.mirror, commit, path, run.options))
            .collect();
        for (path, result) in missing.iter().zip(results) {
            cache.store(commit, path, result?)?;
        }
        if let Some(writer) = &mut cache.writer {
            writer.flush()?;
        }
        blamed += missing.len();
        let mut touched: BTreeSet<CubeKey> = BTreeSet::new();
        let removed: Vec<String> = state
            .keys()
            .filter(|path| !files.contains_key(*path))
            .cloned()
            .collect();
        for path in removed.iter().chain(changed.iter().copied()) {
            if let Some((_, cube)) = state.remove(path) {
                for (key, lines) in cube {
                    *totals.entry(key.clone()).or_default() -= lines;
                    touched.insert(key);
                }
            }
        }
        for path in &changed {
            let (language, kind) = rules
                .classify(run.name, path)
                .ok_or_else(|| anyhow!("unclassified path {path}"))?;
            let hist = &cache.entries[&(commit.to_string(), (*path).clone())];
            let mut cube: BTreeMap<CubeKey, i64> = BTreeMap::new();
            for (sha, lines) in hist {
                let (person, cohort) = attribution.of(sha)?;
                *cube
                    .entry((person, language.clone(), kind.clone(), cohort))
                    .or_default() += *lines as i64;
            }
            for (key, lines) in &cube {
                *totals.entry(key.clone()).or_default() += *lines;
                touched.insert(key.clone());
            }
            state.insert((*path).clone(), (files[*path].clone(), cube));
        }
        for key in touched {
            let value = totals[&key];
            let points = series.entry(key).or_default();
            if points.last() != Some(&value) {
                points.extend([index as i64, value]);
            }
        }
        if interactive {
            eprint!(
                "\r{}: {}/{} samples, {blamed} files blamed   ",
                run.name,
                index + 1,
                picks.len()
            );
        }
    }
    if interactive {
        eprintln!();
    } else {
        eprintln!("{}: {blamed} files blamed", run.name);
    }
    Ok(series)
}

/// Undoes the quoting git applies to paths holding a quote, a backslash, a tab or a newline.
fn unquoted(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return path.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

pub fn rename_target(path: &str) -> String {
    let path = unquoted(path);
    let path = path.as_str();
    if !path.contains("=>") {
        return path.to_string();
    }
    if path.contains('{') {
        let rule = Regex::new(r"\{[^{}]*=> ?([^{}]*)\}").expect("static pattern");
        return rule.replace_all(path, "$1").replace("//", "/");
    }
    path.split_once("=> ")
        .map_or(path, |(_, target)| target)
        .to_string()
}

type LineCounts = BTreeMap<(String, String, String), (i64, i64)>;
type CommitCounts = BTreeMap<(String, String, u32), u64>;
type RepoKey<T> = (String, T);
type RepoLineCounts = BTreeMap<RepoKey<(String, String, String)>, (i64, i64)>;

fn read_activity(
    run: &RepoRun,
    sampled: &str,
    rules: &mut PathRules,
    attribution: &mut Attribution,
) -> Result<(LineCounts, CommitCounts)> {
    let output = git::read(
        Some(run.mirror),
        &[
            "log",
            "--no-merges",
            "--numstat",
            "-M",
            "--format=@%H",
            sampled,
        ],
    )?;
    let mut lines = LineCounts::new();
    let mut commits = CommitCounts::new();
    let mut current: Option<(String, String)> = None;
    let mut mask = 0u32;
    let mut close = |current: &Option<(String, String)>, mask: u32| {
        if let Some((person, month)) = current
            && mask != 0
        {
            *commits
                .entry((person.clone(), month.clone(), mask))
                .or_default() += 1;
        }
    };
    for line in output.split('\n') {
        if let Some(sha) = line.strip_prefix('@') {
            close(&current, mask);
            mask = 0;
            let time = attribution.commits.get(sha).map_or(0, |commit| commit.time);
            current = Some((attribution.of(sha)?.0, dates::month_of(time)));
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let [added, deleted, path] = fields[..] else {
            continue;
        };
        let (Ok(added), Ok(deleted)) = (added.parse::<i64>(), deleted.parse::<i64>()) else {
            continue;
        };
        let Some((_, kind)) = rules.classify(run.name, &rename_target(path)) else {
            continue;
        };
        mask |= 1 << kind_bit(&kind);
        if let Some((person, month)) = &current {
            let entry = lines
                .entry((person.clone(), month.clone(), kind))
                .or_default();
            entry.0 += added;
            entry.1 += deleted;
        }
    }
    close(&current, mask);
    Ok((lines, commits))
}

fn import_rule(config: &Config, repo: &str) -> Result<Option<Regex>> {
    let pattern = config
        .repos
        .get(repo)
        .and_then(|rules| rules.import_subject.as_ref())
        .or(config.rules.import_subject.as_ref());
    pattern
        .map(|pattern| {
            Regex::new(pattern).with_context(|| format!("invalid import_subject `{pattern}`"))
        })
        .transpose()
}

fn index_of<T: Ord + Clone>(values: &BTreeSet<T>) -> BTreeMap<T, usize> {
    values
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, value)| (value, index))
        .collect()
}

pub struct Outcome {
    pub manifest: Manifest,
    pub data: Value,
    pub identities: Value,
    pub merged_identities: usize,
    pub summary: Value,
}

/// Computes the whole analysis from pinned commits. Every collection below is ordered, so the same
/// commits, configuration and git version always give the same bytes.
pub fn analyze(mut manifest: Manifest, fresh: bool) -> Result<Outcome> {
    manifest.validate()?;
    let config = manifest.config.clone();
    let until = dates::parse_date(&manifest.until)?;
    let options = config.blame_options();
    let mut rules = PathRules::new(&config)?;
    let mirrors: Vec<PathBuf> = manifest
        .repos
        .iter()
        .map(|repo| sources::mirror_path(&repo.url))
        .collect();
    let mut chains = Vec::new();
    for (repo, mirror) in manifest.repos.iter().zip(&mirrors) {
        chains.push(first_parent_chain(mirror, &repo.tip)?);
    }
    let start = chains
        .iter()
        .flatten()
        .map(|(_, time)| dates::day_of(*time))
        .min()
        .ok_or_else(|| anyhow!("no commit to analyze"))?;
    let sample_days = dates::sample_dates(start.min(until), until, manifest.interval_weeks);
    // Everything about people comes from the last sampled commit, not from the branch tip:
    // the same sampled commits must give the same report, whatever landed since.
    let all_picks: Vec<Vec<Option<String>>> = chains
        .iter()
        .map(|chain| pick_samples(chain, &sample_days))
        .collect();
    let mut histories = Vec::new();
    let mut seen: BTreeMap<Identity, u64> = BTreeMap::new();
    for (picks, mirror) in all_picks.iter().zip(&mirrors) {
        let commits = match picks.iter().rev().flatten().next() {
            Some(sampled) => read_commits(mirror, sampled)?,
            None => HashMap::new(),
        };
        for commit in commits.values() {
            *seen
                .entry((commit.name.clone(), commit.email.clone()))
                .or_default() += 1;
        }
        histories.push(commits);
    }
    let identities = Identities::new(&config.person, &seen);

    let mut cube: BTreeMap<(String, CubeKey), Vec<i64>> = BTreeMap::new();
    let mut activity: RepoLineCounts = BTreeMap::new();
    let mut commit_counts: BTreeMap<RepoKey<(String, String, u32)>, u64> = BTreeMap::new();
    for ((repo, mirror), (picks, commits)) in manifest
        .repos
        .iter_mut()
        .zip(&mirrors)
        .zip(all_picks.iter().zip(&histories))
    {
        repo.sampled = picks.iter().rev().flatten().next().cloned();
        let run = RepoRun {
            name: &repo.name,
            mirror,
            options: &options,
            fresh,
        };
        let mut attribution = Attribution {
            commits,
            identities: &identities,
            import_subject: import_rule(&config, &repo.name)?,
            memo: HashMap::new(),
        };
        for (key, points) in walk(&run, picks, &mut rules, &mut attribution)? {
            cube.insert((repo.name.clone(), key), points);
        }
        if let Some(sampled) = &repo.sampled {
            let (lines, counts) = read_activity(&run, sampled, &mut rules, &mut attribution)?;
            for (key, value) in lines {
                activity.insert((repo.name.clone(), key), value);
            }
            for (key, value) in counts {
                commit_counts.insert((repo.name.clone(), key), value);
            }
        }
    }

    let repo_index: BTreeMap<&str, usize> = manifest
        .repos
        .iter()
        .enumerate()
        .map(|(i, r)| (r.name.as_str(), i))
        .collect();
    let people: BTreeSet<String> = cube
        .keys()
        .map(|(_, key)| key.0.clone())
        .chain(activity.keys().map(|(_, key)| key.0.clone()))
        .chain(commit_counts.keys().map(|(_, key)| key.0.clone()))
        .collect();
    let languages: BTreeSet<String> = cube.keys().map(|(_, key)| key.1.clone()).collect();
    let cohorts: BTreeSet<String> = cube.keys().map(|(_, key)| key.3.clone()).collect();
    let months: BTreeSet<String> = activity
        .keys()
        .map(|(_, key)| key.1.clone())
        .chain(commit_counts.keys().map(|(_, key)| key.1.clone()))
        .collect();
    let (person_index, language_index, cohort_index, month_index) = (
        index_of(&people),
        index_of(&languages),
        index_of(&cohorts),
        index_of(&months),
    );

    let mut series: Vec<Value> = Vec::new();
    for ((repo, (person, language, kind, cohort)), points) in &cube {
        if points.iter().skip(1).step_by(2).any(|value| *value != 0) {
            series.push(json!([
                repo_index[repo.as_str()],
                person_index[person],
                language_index[language],
                kind_bit(kind),
                cohort_index[cohort],
                points
            ]));
        }
    }
    let activity_rows: Vec<Value> = activity
        .iter()
        .map(|((repo, (person, month, kind)), (added, deleted))| {
            json!([
                repo_index[repo.as_str()],
                person_index[person],
                month_index[month],
                kind,
                added,
                deleted
            ])
        })
        .collect();
    let commit_rows: Vec<Value> = commit_counts
        .iter()
        .map(|((repo, (person, month, mask)), count)| {
            json!([
                repo_index[repo.as_str()],
                person_index[person],
                month_index[month],
                mask,
                count
            ])
        })
        .collect();

    let defaults = default_kinds(&config);
    let kinds: Vec<Value> = KINDS
        .iter()
        .map(|(key, label)| json!({"key": key, "label": label, "default": defaults.iter().any(|kind| kind == key)}))
        .collect();
    let people_rows: Vec<Value> = people
        .iter()
        .map(|person| json!({"name": person, "type": identities.type_of(person)}))
        .collect();
    let mut events: Vec<Value> = config.event.iter().map(|event| json!(event)).collect();
    if config.model_events != Some(false) {
        events.extend(serde_json::from_str::<Vec<Value>>(MODEL_EVENTS)?);
    }
    events.sort_by_key(|event| {
        (
            event["date"].as_str().unwrap_or("").to_string(),
            event["label"].as_str().unwrap_or("").to_string(),
        )
    });
    let groups: BTreeSet<&str> = manifest
        .repos
        .iter()
        .map(|repo| repo.group.as_str())
        .collect();
    let title = config
        .title
        .clone()
        .unwrap_or_else(|| match groups.iter().next() {
            Some(group) if groups.len() == 1 => group.to_string(),
            _ => "All repositories".to_string(),
        });
    let tips: BTreeMap<&str, &str> = manifest
        .repos
        .iter()
        .map(|repo| {
            (
                repo.name.as_str(),
                repo.sampled.as_deref().unwrap_or(repo.tip.as_str()),
            )
        })
        .collect();
    let skipped_names: Vec<&str> = manifest
        .skipped
        .iter()
        .map(|skipped| skipped.name.as_str())
        .collect();
    let data = json!({
        "title": title,
        "manifest": {
            "tool_version": manifest.tool_version,
            "git": manifest.git,
            "until": manifest.until,
            "interval_weeks": manifest.interval_weeks,
            "blame_options": options,
            "commits": tips,
            "skipped": skipped_names,
        },
        "dates": sample_days.iter().map(|day| dates::format_date(*day)).collect::<Vec<_>>(),
        "repos": manifest.repos.iter().map(|repo| json!({"name": repo.name, "group": repo.group})).collect::<Vec<_>>(),
        "people": people_rows,
        "languages": languages,
        "kinds": kinds,
        "cohorts": cohorts,
        "months": months,
        "events": events,
        "series": series,
        "activity": activity_rows,
        "commits": commit_rows,
    });
    let summary = summarize(&cube, &defaults);
    Ok(Outcome {
        manifest,
        data,
        identities: identities.report(),
        merged_identities: identities.merged_count(),
        summary,
    })
}

fn summarize(cube: &BTreeMap<(String, CubeKey), Vec<i64>>, defaults: &[String]) -> Value {
    let mut by_repo: BTreeMap<&str, i64> = BTreeMap::new();
    let mut by_person: BTreeMap<&str, i64> = BTreeMap::new();
    for ((repo, (person, _, kind, _)), points) in cube {
        if defaults.iter().any(|default| default == kind)
            && let Some(lines) = points.last()
        {
            *by_repo.entry(repo).or_default() += lines;
            *by_person.entry(person).or_default() += lines;
        }
    }
    let ranked = |map: BTreeMap<&str, i64>, limit: usize| {
        let mut rows: Vec<(&str, i64)> = map.into_iter().filter(|(_, lines)| *lines > 0).collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        rows.truncate(limit);
        rows.into_iter()
            .map(|(name, lines)| json!({"name": name, "lines": lines}))
            .collect::<Vec<_>>()
    };
    let total: i64 = by_repo.values().sum();
    json!({"surviving_lines": total, "by_repo": ranked(by_repo, usize::MAX), "top_people": ranked(by_person, 8)})
}

pub fn numstat_between(mirror: &Path, from: Option<&str>, to: &str) -> Result<Vec<(i64, String)>> {
    let empty_tree = if to.len() == 64 {
        EMPTY_TREE_SHA256
    } else {
        EMPTY_TREE_SHA1
    };
    let output = git::read(
        Some(mirror),
        &["diff", "--numstat", "-M", from.unwrap_or(empty_tree), to],
    )?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [added, deleted, path] = fields[..] else {
                return None;
            };
            Some((
                added.parse::<i64>().ok()? - deleted.parse::<i64>().ok()?,
                rename_target(path),
            ))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_hunk_headers_from_incremental_blame() {
        let sha = "a".repeat(40);
        let output = format!(
            "{sha} 1 1 3\nauthor Ada Lovelace Jr\nsummary a b c\nfilename x\n{sha} 4 4 2\nfilename x\n"
        );
        assert_eq!(parse_blame(&output)[&sha], 5);
    }

    #[test]
    fn ignores_a_header_shaped_like_a_hunk() {
        let output = "author-tz +0100 1 1\nsummary 1 2 3\n";
        assert!(parse_blame(output).is_empty());
    }

    #[test]
    fn undoes_git_path_quoting() {
        assert_eq!(rename_target(r#""a\tb \"c\".rs""#), "a\tb \"c\".rs");
    }

    #[test]
    fn refuses_a_manifest_pointing_at_anything_but_a_commit() {
        let repo = ManifestRepo {
            name: "api".into(),
            group: "g".into(),
            url: "git@h:a.git".into(),
            branch: "main".into(),
            tip: "--output=/tmp/x".into(),
            sampled: None,
            alternate: None,
        };
        let manifest = Manifest {
            tool_version: "0".into(),
            git: "git".into(),
            until: "2024-01-01".into(),
            interval_weeks: 1,
            config: Config::default(),
            repos: vec![repo],
            skipped: vec![],
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn resolves_rename_notation() {
        assert_eq!(rename_target("src/{a => b}/c.rs"), "src/b/c.rs");
        assert_eq!(rename_target("src/{a => }/c.rs"), "src/c.rs");
        assert_eq!(rename_target("old.rs => new.rs"), "new.rs");
        assert_eq!(rename_target("plain.rs"), "plain.rs");
    }

    #[test]
    fn picks_the_newest_commit_made_by_each_date() {
        let chain = vec![
            ("c".to_string(), 3 * 86_400),
            ("b".to_string(), 2 * 86_400),
            ("a".to_string(), 86_400),
        ];
        let picks = pick_samples(&chain, &[0, 2, 9]);
        assert_eq!(picks, [None, Some("b".to_string()), Some("c".to_string())]);
    }

    #[test]
    fn tolerates_committer_dates_going_backwards() {
        let chain = vec![("new".to_string(), 86_400), ("old".to_string(), 5 * 86_400)];
        let picks = pick_samples(&chain, &[2, 9]);
        assert_eq!(picks, [Some("new".to_string()), Some("new".to_string())]);
    }
}
