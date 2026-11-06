use crate::config::SourceConfig;
use crate::git;
use crate::output;
use crate::rules::glob_regex;
use anyhow::{Context, Result, anyhow, bail};
use regex::{Regex, RegexSet};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub name: String,
    pub group: String,
    pub url: String,
    pub alternate: Option<String>,
}

type Synced = (String, PathBuf, Option<(String, String)>);

const FETCHED_REF: &str = "refs/planks/head";

#[derive(Debug, PartialEq, Eq)]
struct Location {
    url: String,
    alternate: Option<String>,
}

/// How sources turn into repositories. `trusted_hosts` only ever comes from the command line:
/// a config file found in a checkout must not be able to send a token to a host of its choice.
pub struct Discovery<'a> {
    pub protocol: Protocol,
    pub keep_going: bool,
    pub trusted_hosts: &'a [String],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Protocol {
    Auto,
    Ssh,
    Https,
}

#[derive(Debug, PartialEq, Eq)]
struct Remote {
    name: String,
    ssh: String,
    https: String,
}

pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GIT_PLANKS_CACHE") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("git-planks");
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map_or_else(
        || PathBuf::from(".git-planks-cache"),
        |home| PathBuf::from(home).join(".cache/git-planks"),
    )
}

/// Splits `group=source`. A `=` after a `/` or `:` belongs to the source itself.
pub fn split_group(argument: &str) -> (Option<&str>, &str) {
    if let Some((group, source)) = argument.split_once('=')
        && !group.is_empty()
        && group
            .chars()
            .all(|c| c.is_alphanumeric() || "_.-".contains(c))
    {
        return (Some(group), source);
    }
    (None, argument)
}

pub fn is_remote_url(url: &str) -> bool {
    let scheme = ["https://", "http://", "ssh://", "git://", "git+ssh://"];
    if scheme.iter().any(|prefix| url.starts_with(prefix)) {
        return true;
    }
    // `host:path` and `user@host:path`, as in git. A bare host needs two characters to leave Windows
    // drives out, and `file:///` and `ext::` stay refused.
    Regex::new(
        r"^([A-Za-z0-9][A-Za-z0-9._-]*@[A-Za-z0-9.-]+|[A-Za-z0-9][A-Za-z0-9.-]+):(/[^/:]|[^:/\\-])",
    )
    .expect("static pattern")
    .is_match(url)
}

/// Removes credentials, so that a manifest can be shared.
pub fn without_credentials(url: &str) -> String {
    Regex::new(r"^(https?://)[^/@]+@")
        .expect("static pattern")
        .replace(url, "$1")
        .into_owned()
}

fn after_user(text: &str) -> &str {
    match text.split_once('@') {
        Some((user, rest)) if !user.contains('/') => rest,
        _ => text,
    }
}

/// `host/path` of a remote, or the path of a folder, with its original case.
fn canonical(url: &str) -> String {
    let text = without_credentials(url);
    let joined = if let Some((_, rest)) = text.split_once("://") {
        let rest = after_user(rest);
        match rest.split_once('/') {
            Some((host, path)) => format!("{}/{path}", host.split(':').next().unwrap_or(host)),
            None => rest.to_string(),
        }
    } else if is_remote_url(&text) {
        after_user(&text).replacen(':', "/", 1).replace("//", "/")
    } else {
        text.clone()
    };
    joined
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

/// Reduces any address of one repository to the same key: forges ignore case in paths.
pub fn normalize(url: &str) -> String {
    let key = canonical(url);
    if is_remote_url(url) {
        key.to_lowercase()
    } else {
        key
    }
}

fn name_from_url(url: &str) -> String {
    canonical(url)
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("repo")
        .to_string()
}

fn owner_from_url(url: &str) -> Option<String> {
    let key = canonical(url);
    let parts: Vec<&str> = key.split('/').collect();
    (parts.len() >= 3).then(|| parts[parts.len() - 2].to_string())
}

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .user_agent(concat!("git-planks/", env!("CARGO_PKG_VERSION")))
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

pub fn get_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, String)],
) -> Result<(u16, Value)> {
    let mut request = agent.get(url);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let mut response = request
        .call()
        .with_context(|| format!("cannot reach {url}"))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("cannot read {url}"))?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

fn access_error(service: &str, status: u16, target: &str, env: &str) -> anyhow::Error {
    let hint = match status {
        401 | 403 => {
            format!("access refused or rate limited: set {env} to a token that can read it")
        }
        404 => format!("not found, or private: check the name, or set {env}"),
        301 | 302 | 307 | 308 => "the address moved: use its new name".to_string(),
        _ => format!("unexpected status {status}"),
    };
    anyhow!("{service} `{target}`: {hint}")
}

fn str_field(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn github_token() -> Option<String> {
    if let Some(token) = env_token(&["GITHUB_TOKEN", "GH_TOKEN"]) {
        return Some(token);
    }
    let output = Command::new("gh").args(["auth", "token"]).output().ok()?;
    let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && !token.is_empty()).then_some(token)
}

fn env_token(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|token| !token.is_empty()))
}

fn github_list(base: &str, owner: &str, token: Option<&str>) -> Result<Vec<Remote>> {
    let agent = agent();
    let mut headers = vec![("Accept", "application/vnd.github+json".to_string())];
    if let Some(token) = token {
        headers.push(("Authorization", format!("Bearer {token}")));
    }
    let mut endpoint = format!("{base}/orgs/{owner}/repos?type=all");
    if token.is_some() {
        let (status, user) = get_json(&agent, &format!("{base}/user"), &headers)?;
        if status == 200 && str_field(&user, "/login").eq_ignore_ascii_case(owner) {
            endpoint = format!("{base}/user/repos?affiliation=owner");
        }
    }
    let mut remotes = Vec::new();
    let mut page = 1;
    loop {
        let url = format!("{endpoint}&per_page=100&page={page}");
        let (mut status, mut body) = get_json(&agent, &url, &headers)?;
        if status == 404 && page == 1 && endpoint.contains("/orgs/") {
            endpoint = format!("{base}/users/{owner}/repos?type=owner");
            (status, body) =
                get_json(&agent, &format!("{endpoint}&per_page=100&page=1"), &headers)?;
        }
        if status != 200 {
            return Err(access_error("GitHub", status, owner, "GITHUB_TOKEN"));
        }
        let items = body.as_array().cloned().unwrap_or_default();
        for item in &items {
            let skip = item["archived"].as_bool().unwrap_or(false)
                || item["fork"].as_bool().unwrap_or(false);
            if !skip {
                remotes.push(Remote {
                    name: str_field(item, "/name"),
                    ssh: str_field(item, "/ssh_url"),
                    https: str_field(item, "/clone_url"),
                });
            }
        }
        if items.len() < 100 {
            return Ok(remotes);
        }
        page += 1;
    }
}

fn percent_encode_path(path: &str) -> String {
    path.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn gitlab_list(base: &str, group: &str, token: Option<&str>) -> Result<Vec<Remote>> {
    let agent = agent();
    let headers: Vec<(&str, String)> = token
        .map(|t| ("PRIVATE-TOKEN", t.to_string()))
        .into_iter()
        .collect();
    let encoded = percent_encode_path(group);
    let mut endpoint =
        format!("{base}/api/v4/groups/{encoded}/projects?include_subgroups=true&archived=false");
    let mut remotes = Vec::new();
    let mut page = 1;
    loop {
        let url = format!("{endpoint}&order_by=id&sort=asc&per_page=100&page={page}");
        let (mut status, mut body) = get_json(&agent, &url, &headers)?;
        if status == 404 && page == 1 && !group.contains('/') {
            endpoint = format!("{base}/api/v4/users/{encoded}/projects?archived=false");
            let url = format!("{endpoint}&order_by=id&sort=asc&per_page=100&page=1");
            (status, body) = get_json(&agent, &url, &headers)?;
        }
        if status != 200 {
            return Err(access_error("GitLab", status, group, "GITLAB_TOKEN"));
        }
        let items = body.as_array().cloned().unwrap_or_default();
        for item in &items {
            if item.get("forked_from_project").is_none() {
                remotes.push(Remote {
                    name: str_field(item, "/path"),
                    ssh: str_field(item, "/ssh_url_to_repo"),
                    https: str_field(item, "/http_url_to_repo"),
                });
            }
        }
        if items.len() < 100 {
            return Ok(remotes);
        }
        page += 1;
    }
}

fn bitbucket_list(base: &str, workspace: &str, headers: &[(&str, String)]) -> Result<Vec<Remote>> {
    let agent = agent();
    let mut url = format!("{base}/repositories/{workspace}?pagelen=100");
    let mut remotes = Vec::new();
    loop {
        let (status, body) = get_json(&agent, &url, headers)?;
        if status != 200 {
            return Err(access_error(
                "Bitbucket",
                status,
                workspace,
                "BITBUCKET_TOKEN",
            ));
        }
        for item in body["values"].as_array().cloned().unwrap_or_default() {
            if item.get("parent").is_some_and(|parent| !parent.is_null()) {
                continue;
            }
            let link = |protocol: &str| {
                item["links"]["clone"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|clone| clone["name"] == protocol)
                    .map(|clone| str_field(clone, "/href"))
                    .unwrap_or_default()
            };
            remotes.push(Remote {
                name: str_field(&item, "/slug"),
                ssh: link("ssh"),
                https: without_credentials(&link("https")),
            });
        }
        match body["next"].as_str() {
            // The next page comes from the server: the token is only ever sent back to the same API.
            Some(next) if next.starts_with(base) => url = next.to_string(),
            Some(next) => {
                bail!("Bitbucket `{workspace}`: refusing the next page outside the API: {next}")
            }
            None => return Ok(remotes),
        }
    }
}

fn bitbucket_headers() -> Vec<(&'static str, String)> {
    if let Some(token) = env_token(&["BITBUCKET_TOKEN"]) {
        return vec![("Authorization", format!("Bearer {token}"))];
    }
    Vec::new()
}

/// Splits `https://host/path`, the only way to name a self-hosted instance: a dot in the first
/// segment of `gitlab:acme.io/team` is a group name on gitlab.com, never a host.
fn self_hosted(spec: &str) -> Result<Option<(&str, &str)>> {
    if spec.starts_with("http://") {
        bail!("`{spec}`: an API token must not travel over plain http, use https://");
    }
    let Some(rest) = spec.strip_prefix("https://") else {
        return Ok(None);
    };
    match rest.split_once('/') {
        Some((host, path)) if !host.is_empty() && !host.contains('@') && !path.is_empty() => {
            Ok(Some((host, path.trim_end_matches('/'))))
        }
        _ => bail!("`{spec}`: expected https://host/group"),
    }
}

/// A token only reaches its own forge, or a host the user named with `--trust-host`.
fn token_for(host: &str, names: &[&str], trusted_hosts: &[String]) -> Option<String> {
    let token = env_token(names)?;
    if trusted_hosts
        .iter()
        .any(|trusted| trusted.eq_ignore_ascii_case(host))
    {
        return Some(token);
    }
    eprintln!(
        "note: {} is not sent to {host}: pass --trust-host {host} to send it",
        names[0]
    );
    None
}

fn choose_location(remote: &Remote, protocol: Protocol, authenticated: bool) -> Location {
    let (url, alternate) = match protocol {
        Protocol::Ssh => (&remote.ssh, None),
        Protocol::Https => (&remote.https, None),
        Protocol::Auto if authenticated => (&remote.ssh, Some(&remote.https)),
        Protocol::Auto => (&remote.https, Some(&remote.ssh)),
    };
    let alternate = alternate.filter(|other| !other.is_empty()).cloned();
    if url.is_empty() {
        return Location {
            url: alternate.unwrap_or_default(),
            alternate: None,
        };
    }
    Location {
        url: url.clone(),
        alternate,
    }
}

fn checked_location(remote: &Remote, protocol: Protocol, authenticated: bool) -> Result<Location> {
    let mut location = choose_location(remote, protocol, authenticated);
    location.alternate = location.alternate.filter(|other| is_remote_url(other));
    if !is_remote_url(&location.url) {
        bail!("the API returned an unusable address for `{}`", remote.name);
    }
    Ok(location)
}

fn expand_platform(spec: &str, discovery: &Discovery) -> Result<Option<(String, Vec<Location>)>> {
    let (remotes, owner, authenticated) = if let Some(rest) = spec.strip_prefix("github:") {
        let names = ["GITHUB_TOKEN", "GH_TOKEN"];
        let (base, owner, token) = match self_hosted(rest)? {
            Some((host, owner)) => (
                format!("https://{host}/api/v3"),
                owner,
                token_for(host, &names, discovery.trusted_hosts),
            ),
            None => ("https://api.github.com".to_string(), rest, github_token()),
        };
        (
            github_list(&base, owner, token.as_deref())?,
            owner,
            token.is_some(),
        )
    } else if let Some(rest) = spec.strip_prefix("gitlab:") {
        let names = ["GITLAB_TOKEN"];
        let (base, group, token) = match self_hosted(rest)? {
            Some((host, group)) => (
                format!("https://{host}"),
                group,
                token_for(host, &names, discovery.trusted_hosts),
            ),
            None => ("https://gitlab.com".to_string(), rest, env_token(&names)),
        };
        (
            gitlab_list(&base, group, token.as_deref())?,
            group,
            token.is_some(),
        )
    } else if let Some(workspace) = spec.strip_prefix("bitbucket:") {
        let headers = bitbucket_headers();
        let authenticated = !headers.is_empty();
        (
            bitbucket_list("https://api.bitbucket.org/2.0", workspace, &headers)?,
            workspace,
            authenticated,
        )
    } else {
        return Ok(None);
    };
    if remotes.is_empty() {
        bail!(
            "`{spec}` lists no repository: it is empty, or its repositories are private and no token gives access"
        );
    }
    let locations = remotes
        .iter()
        .map(|remote| checked_location(remote, discovery.protocol, authenticated))
        .collect::<Result<Vec<_>>>()?;
    let group = owner.rsplit('/').next().unwrap_or(owner).to_string();
    Ok(Some((group, locations)))
}

fn read_list_file(path: &str) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn expand_one(spec: &str, discovery: &Discovery) -> Result<(Option<String>, Vec<Location>)> {
    let typed = |url: String| Location {
        url,
        alternate: None,
    };
    if let Some((group, locations)) = expand_platform(spec, discovery)? {
        return Ok((Some(group), locations));
    }
    if let Some(path) = spec.strip_prefix('@') {
        return Ok((None, read_list_file(path)?.into_iter().map(typed).collect()));
    }
    Ok((None, vec![typed(spec.to_string())]))
}

fn resolve_location(location: &str) -> Result<(String, bool)> {
    if location.starts_with('-') {
        bail!("invalid source `{location}`");
    }
    if is_remote_url(location) {
        return Ok((location.to_string(), false));
    }
    let path = Path::new(location);
    if path.exists() {
        let absolute = path
            .canonicalize()
            .with_context(|| format!("cannot resolve {location}"))?;
        let text = absolute.to_string_lossy();
        let text = if cfg!(windows) {
            text.trim_start_matches(r"\\?\").replace('\\', "/")
        } else {
            text.into_owned()
        };
        return Ok((text, true));
    }
    bail!(
        "`{location}` is neither a folder nor a git address. Use a path, git@host:org/repo.git, https://host/org/repo.git, github:owner, gitlab:group or bitbucket:workspace"
    )
}

/// Resolves a `.gitmodules` address, which may be relative to its parent. Only remote addresses
/// come out: a repository must not be able to pull a local folder into the report.
pub fn resolve_submodule_url(parent: &str, url: &str) -> Option<String> {
    let mut relative = url;
    if !(relative.starts_with("./") || relative.starts_with("../")) {
        return is_remote_url(url).then(|| url.to_string());
    }
    if !is_remote_url(parent) {
        return None;
    }
    let trimmed = parent.trim_end_matches('/').trim_end_matches(".git");
    let (scp_prefix, mut path) = match (trimmed.contains("://"), trimmed.split_once(':')) {
        (false, Some((host, path))) => (Some(host), path.to_string()),
        _ => (None, trimmed.to_string()),
    };
    while let Some(rest) = relative.strip_prefix("./") {
        relative = rest;
    }
    while let Some(rest) = relative.strip_prefix("../") {
        relative = rest;
        match path.rfind('/') {
            Some(index) => path.truncate(index),
            None if !path.is_empty() => path.clear(),
            None => return None,
        }
    }
    let joined = if path.is_empty() {
        relative.to_string()
    } else {
        format!("{path}/{relative}")
    };
    let resolved = scp_prefix.map_or(joined.clone(), |host| format!("{host}:{joined}"));
    is_remote_url(&resolved).then_some(resolved)
}

pub fn submodule_urls(mirror: &Path, tip: &str, parent: &str) -> Result<Vec<String>> {
    let blob = format!("{tip}:.gitmodules");
    if git::read(Some(mirror), &["cat-file", "-e", &blob]).is_err() {
        return Ok(Vec::new());
    }
    let listing = git::read(
        Some(mirror),
        &[
            "config",
            "--null",
            "--blob",
            &blob,
            "--get-regexp",
            r"^submodule\..*\.url$",
        ],
    )
    .unwrap_or_default();
    // Relative addresses of a local checkout follow its `origin`, as git does.
    let base = if is_remote_url(parent) {
        parent.to_string()
    } else {
        git::read(Some(Path::new(parent)), &["remote", "get-url", "origin"])
            .map(|url| url.trim().to_string())
            .unwrap_or_default()
    };
    let mut urls = Vec::new();
    for entry in listing.split('\0') {
        let Some((_, url)) = entry.split_once('\n') else {
            continue;
        };
        match resolve_submodule_url(&base, url.trim()) {
            Some(resolved) => urls.push(resolved),
            None => eprintln!(
                "warning: submodule address not followed: {}",
                output::printable(url.trim())
            ),
        }
    }
    Ok(urls)
}

pub fn mirror_path(url: &str) -> PathBuf {
    let key = normalize(url);
    let digest = Sha256::digest(key.as_bytes());
    let hash: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let slug: String = name_from_url(url)
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cache_dir()
        .join("mirrors")
        .join(format!("{slug}-{hash}.git"))
}

/// Brings the mirror of `url` up to date with what the remote serves as `HEAD`, and returns that
/// branch and its tip, or `None` for an empty repository. An unreachable remote is an error.
pub fn sync_mirror(url: &str) -> Result<(PathBuf, Option<(String, String)>)> {
    let mirror = mirror_path(url);
    if mirror.exists() {
        git::network(Some(&mirror), &["remote", "set-url", "origin", "--", url])?;
    } else {
        std::fs::create_dir_all(cache_dir().join("mirrors"))?;
        let partial = format!("{}.partial-{}", mirror.display(), std::process::id());
        git::read(None, &["init", "--quiet", "--bare", &partial])?;
        git::network(
            Some(Path::new(&partial)),
            &["remote", "add", "origin", "--", url],
        )?;
        std::fs::rename(&partial, &mirror)?;
    }
    let head = git::network(Some(&mirror), &["ls-remote", "--symref", "origin", "HEAD"])?;
    if head.trim().is_empty() {
        let heads = git::network(Some(&mirror), &["ls-remote", "--heads", "origin"])?;
        if heads.trim().is_empty() {
            return Ok((mirror, None));
        }
        bail!(
            "{} has branches but no default branch",
            without_credentials(url)
        );
    }
    let symbolic = head
        .lines()
        .find_map(|line| line.strip_prefix("ref: refs/heads/")?.split_once('\t'))
        .map(|(branch, _)| branch.to_string());
    let source = symbolic
        .as_ref()
        .map_or("HEAD".to_string(), |branch| format!("refs/heads/{branch}"));
    let refspec = format!("+{source}:{FETCHED_REF}");
    git::network(
        Some(&mirror),
        &["fetch", "--quiet", "--no-tags", "origin", &refspec],
    )?;
    let shallow = git::read(Some(&mirror), &["rev-parse", "--is-shallow-repository"])?;
    if shallow.trim() == "true" {
        bail!(
            "{} is a shallow clone: its history is cut, so every line would be credited to the last commit. Run `git fetch --unshallow` there (in GitHub Actions: `fetch-depth: 0`)",
            without_credentials(url)
        );
    }
    git::read(
        Some(&mirror),
        &[
            "commit-graph",
            "write",
            "--reachable",
            "--changed-paths",
            "--split",
        ],
    )?;
    let tip = git::read(Some(&mirror), &["rev-parse", "--verify", FETCHED_REF])?
        .trim()
        .to_string();
    keep_commit(&mirror, &tip)?;
    Ok((
        mirror,
        Some((symbolic.unwrap_or_else(|| "HEAD".to_string()), tip)),
    ))
}

/// Keeps an analyzed commit out of garbage collection, so that its report stays verifiable after a force push.
fn keep_commit(mirror: &Path, sha: &str) -> Result<()> {
    let reference = format!("refs/planks/analyzed/{sha}");
    git::read(Some(mirror), &["update-ref", &reference, sha]).map(|_| ())
}

/// Syncs from the preferred address, then from the other protocol when `auto` offered one.
pub fn sync_repo(url: &str, alternate: Option<&str>) -> Result<Synced> {
    match (sync_mirror(url), alternate) {
        (Ok((mirror, state)), _) => Ok((url.to_string(), mirror, state)),
        (Err(first), Some(other)) => match sync_mirror(other) {
            Ok((mirror, state)) => Ok((other.to_string(), mirror, state)),
            Err(second) => Err(anyhow!("{first:#}\n{second:#}")),
        },
        (Err(first), None) => Err(first),
    }
}

/// Makes a pinned commit available, from the mirror when it already has it.
pub fn ensure_commit(url: &str, alternate: Option<&str>, sha: &str) -> Result<PathBuf> {
    let mirror = mirror_path(url);
    let object = format!("{sha}^{{commit}}");
    let present = || git::read(Some(&mirror), &["cat-file", "-e", &object]).is_ok();
    if mirror.exists() && present() {
        return Ok(mirror);
    }
    sync_repo(url, alternate)?;
    if !present() {
        // Gone from the branch, after a force push: forges still serve it by its id.
        let _ = git::network(
            Some(&mirror),
            &["fetch", "--quiet", "--no-tags", "origin", sha],
        );
    }
    if present() {
        keep_commit(&mirror, sha)?;
        return Ok(mirror);
    }
    bail!(
        "commit {sha} is not reachable from {}",
        without_credentials(url)
    )
}

#[derive(Debug, Clone)]
pub struct Requested {
    pub source: String,
    pub group: Option<String>,
    pub exclude: Vec<String>,
    pub submodules: bool,
}

impl Requested {
    pub fn from_argument(argument: &str, submodules: bool) -> Self {
        let (group, source) = split_group(argument);
        Self {
            source: source.to_string(),
            group: group.map(str::to_string),
            exclude: Vec::new(),
            submodules,
        }
    }

    pub fn from_config(config: &SourceConfig) -> Self {
        Self {
            source: config.from.clone(),
            group: config.group.clone(),
            exclude: config.exclude.clone(),
            submodules: config.submodules,
        }
    }
}

/// Expands every source into a sorted list of repositories, one entry per distinct remote.
pub fn discover(requested: &[Requested], discovery: &Discovery) -> Result<Vec<Repo>> {
    let mut by_key: BTreeMap<String, (Location, String)> = BTreeMap::new();
    for request in requested {
        let (platform_group, locations) = expand_one(&request.source, discovery)?;
        let excluded = RegexSet::new(request.exclude.iter().map(|p| glob_regex(p)))
            .context("invalid repository pattern")?;
        for location in locations {
            let (url, local) = resolve_location(&location.url)?;
            let alternate = location.alternate;
            let name = name_from_url(&url);
            if excluded.is_match(&name) {
                continue;
            }
            let group = request
                .group
                .clone()
                .or_else(|| platform_group.clone())
                .or_else(|| if local { None } else { owner_from_url(&url) })
                .unwrap_or_else(|| "repositories".to_string());
            let key = normalize(&url);
            if request.submodules {
                match sync_repo(&url, alternate.as_deref()) {
                    Ok((used, mirror, Some((_, tip)))) => {
                        for submodule in submodule_urls(&mirror, &tip, &used)? {
                            if !excluded.is_match(&name_from_url(&submodule)) {
                                by_key.entry(normalize(&submodule)).or_insert_with(|| {
                                    let location = Location {
                                        url: submodule,
                                        alternate: None,
                                    };
                                    (location, group.clone())
                                });
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(error) if discovery.keep_going => {
                        eprintln!("warning: {}: {error:#}", output::printable(&name));
                    }
                    Err(error) => return Err(error),
                }
            }
            by_key
                .entry(key)
                .or_insert((Location { url, alternate }, group));
        }
    }
    Ok(name_repos(by_key))
}

/// Names each repository by the fewest path segments that keep every name unique.
fn name_repos(by_key: BTreeMap<String, (Location, String)>) -> Vec<Repo> {
    let entries: Vec<(Location, String)> = by_key.into_values().collect();
    let segments: Vec<Vec<String>> = entries
        .iter()
        .map(|(location, _)| {
            canonical(&location.url)
                .split('/')
                .map(str::to_string)
                .collect()
        })
        .collect();
    let mut depths = vec![1usize; entries.len()];
    let name_at = |index: usize, depth: usize| {
        let parts = &segments[index];
        parts[parts.len().saturating_sub(depth)..].join("/")
    };
    loop {
        let mut holders: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, depth) in depths.iter().enumerate() {
            holders
                .entry(name_at(index, *depth))
                .or_default()
                .push(index);
        }
        let mut grew = false;
        for indices in holders.values().filter(|indices| indices.len() > 1) {
            for index in indices {
                if depths[*index] < segments[*index].len() {
                    depths[*index] += 1;
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    let mut repos: Vec<Repo> = entries
        .into_iter()
        .enumerate()
        .map(|(index, (location, group))| Repo {
            name: name_at(index, depths[index]),
            group,
            url: location.url,
            alternate: location.alternate,
        })
        .collect();
    repos.sort_by(|a, b| a.name.cmp(&b.name));
    repos
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn serve(pages: Vec<(u16, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for (status, body) in pages {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0; 4096];
                let _ = stream.read(&mut buffer);
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        address
    }

    #[test]
    fn splits_an_optional_group() {
        assert_eq!(
            split_group("core=github:acme"),
            (Some("core"), "github:acme")
        );
        assert_eq!(split_group("github:acme"), (None, "github:acme"));
        assert_eq!(split_group("https://h/a?x=1"), (None, "https://h/a?x=1"));
    }

    #[test]
    fn recognizes_remote_addresses() {
        assert!(is_remote_url("git@github.com:acme/api.git"));
        assert!(is_remote_url("https://gitlab.com/acme/api.git"));
        assert!(is_remote_url("ssh://git@host:2222/acme/api.git"));
        assert!(is_remote_url("github.com:acme/api.git"));
        assert!(is_remote_url("my-alias:acme/api.git"));
        assert!(is_remote_url("git@host:/srv/git/api.git"));
    }

    #[test]
    fn refuses_addresses_that_run_or_read_local_things() {
        for address in [
            "ext::sh -c evil",
            "file:///etc",
            "fd::3",
            "-oProxyCommand=evil@host:x",
            "--upload-pack=evil",
            "./local",
            "/abs/path",
            r"C:\Users\me",
            "C:/Users/me",
        ] {
            assert!(!is_remote_url(address), "{address}");
        }
    }

    #[test]
    fn treats_every_address_of_a_repository_as_the_same_one() {
        for address in [
            "git@github.com:acme/api.git",
            "https://github.com/acme/api.git",
            "https://user:secret@github.com/acme/api/",
            "ssh://git@github.com/acme/api",
            "ssh://git@github.com:2222/acme/api",
            "https://github.com/ACME/api",
            "github.com:acme/api",
        ] {
            assert_eq!(normalize(address), "github.com/acme/api", "{address}");
        }
    }

    #[test]
    fn keeps_the_case_of_names_and_folders() {
        assert_eq!(name_from_url("https://github.com/acme/TYPO3.git"), "TYPO3");
        assert_eq!(normalize("/Users/me/Api"), "/Users/me/Api");
    }

    #[test]
    fn reads_self_hosted_instances_only_from_an_explicit_address() {
        assert_eq!(self_hosted("acme.io/team").unwrap(), None);
        assert_eq!(
            self_hosted("https://git.acme.dev/platform/infra").unwrap(),
            Some(("git.acme.dev", "platform/infra"))
        );
        assert!(self_hosted("http://git.acme.dev/platform").is_err());
        assert!(self_hosted("https://git.acme.dev").is_err());
    }

    #[test]
    fn sends_a_token_only_to_a_trusted_host() {
        unsafe { std::env::set_var("GIT_PLANKS_TEST_TOKEN", "secret") };
        let trusted = vec!["git.acme.dev".to_string()];
        let names = ["GIT_PLANKS_TEST_TOKEN"];
        assert_eq!(
            token_for("GIT.acme.dev", &names, &trusted).as_deref(),
            Some("secret")
        );
        assert_eq!(token_for("attacker.example", &names, &trusted), None);
    }

    #[test]
    fn strips_credentials_from_addresses() {
        assert_eq!(
            without_credentials("https://u:tok@host/a.git"),
            "https://host/a.git"
        );
        assert_eq!(without_credentials("git@host:a.git"), "git@host:a.git");
    }

    #[test]
    fn resolves_relative_submodules_on_each_protocol() {
        for (parent, expected) in [
            ("git@host:org/root.git", "git@host:org/api.git"),
            ("https://host/org/root.git", "https://host/org/api.git"),
            ("git@host:root.git", "git@host:api.git"),
            ("alias:org/root", "alias:org/api.git"),
        ] {
            assert_eq!(
                resolve_submodule_url(parent, "../api.git").as_deref(),
                Some(expected),
                "{parent}"
            );
        }
    }

    #[test]
    fn refuses_unsafe_or_local_submodule_addresses() {
        let parent = "git@host:org/root.git";
        for address in [
            "ext::sh -c evil",
            "file:///etc",
            "--upload-pack=evil",
            "/etc",
            "../../../x",
        ] {
            assert_eq!(resolve_submodule_url(parent, address), None, "{address}");
        }
        assert_eq!(resolve_submodule_url("/home/me/root", "../private"), None);
    }

    #[test]
    fn auto_prefers_ssh_with_a_token_and_keeps_the_other_protocol_as_fallback() {
        let remote = Remote {
            name: "a".into(),
            ssh: "git@h:a.git".into(),
            https: "https://h/a.git".into(),
        };
        let location = |protocol, authenticated| choose_location(&remote, protocol, authenticated);
        assert_eq!(location(Protocol::Auto, false).url, "https://h/a.git");
        assert_eq!(
            location(Protocol::Auto, false).alternate.as_deref(),
            Some("git@h:a.git")
        );
        assert_eq!(location(Protocol::Auto, true).url, "git@h:a.git");
        assert_eq!(
            location(Protocol::Auto, true).alternate.as_deref(),
            Some("https://h/a.git")
        );
    }

    #[test]
    fn an_explicit_protocol_has_no_fallback() {
        let remote = Remote {
            name: "a".into(),
            ssh: "git@h:a.git".into(),
            https: "https://h/a.git".into(),
        };
        assert_eq!(
            choose_location(&remote, Protocol::Https, true),
            Location {
                url: "https://h/a.git".into(),
                alternate: None
            }
        );
        assert_eq!(
            choose_location(&remote, Protocol::Ssh, false),
            Location {
                url: "git@h:a.git".into(),
                alternate: None
            }
        );
    }

    #[test]
    fn github_listing_follows_pages_and_skips_forks_and_archives() {
        let first: Vec<Value> = (0..100)
            .map(|i| serde_json::json!({"name": format!("r{i}"), "ssh_url": "s", "clone_url": "c", "archived": i == 0, "fork": i == 1}))
            .collect();
        let second = serde_json::json!([{"name": "last", "ssh_url": "s", "clone_url": "c"}]);
        let base = serve(vec![
            (200, Value::Array(first).to_string()),
            (200, second.to_string()),
        ]);
        let remotes = github_list(&base, "acme", None).unwrap();
        assert_eq!(remotes.len(), 99);
        assert_eq!(remotes.last().unwrap().name, "last");
    }

    #[test]
    fn github_listing_falls_back_from_organization_to_user() {
        let user = serde_json::json!([{"name": "solo", "ssh_url": "s", "clone_url": "c"}]);
        let base = serve(vec![(404, "{}".to_string()), (200, user.to_string())]);
        assert_eq!(github_list(&base, "someone", None).unwrap()[0].name, "solo");
    }

    #[test]
    fn explains_a_refused_listing() {
        let base = serve(vec![(403, "{}".to_string())]);
        let message = github_list(&base, "acme", None).unwrap_err().to_string();
        assert!(message.contains("GITHUB_TOKEN"), "{message}");
    }

    #[test]
    fn gitlab_listing_skips_forks() {
        let body = serde_json::json!([
            {"path": "api", "ssh_url_to_repo": "s", "http_url_to_repo": "h"},
            {"path": "copy", "ssh_url_to_repo": "s", "http_url_to_repo": "h", "forked_from_project": {"id": 1}}
        ]);
        let base = serve(vec![(200, body.to_string())]);
        let remotes = gitlab_list(&base, "acme/platform", None).unwrap();
        assert_eq!(
            remotes.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["api"]
        );
    }

    #[test]
    fn bitbucket_listing_follows_next_links_inside_the_api_only() {
        let repo = |slug: &str| serde_json::json!({"slug": slug, "links": {"clone": [{"name": "ssh", "href": "git@b:a.git"}, {"name": "https", "href": "https://user@b/a.git"}]}});
        let listener_base = serve(vec![(
            200,
            serde_json::json!({"values": [repo("one")], "next": "https://evil.example/steal"})
                .to_string(),
        )]);
        let error = bitbucket_list(&listener_base, "acme", &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("refusing"), "{error}");
        let base = serve(vec![(
            200,
            serde_json::json!({"values": [repo("one")]}).to_string(),
        )]);
        let remotes = bitbucket_list(&base, "acme", &[]).unwrap();
        assert_eq!(remotes[0].https, "https://b/a.git");
    }

    fn names_of(paths: &[&str]) -> Vec<String> {
        let mut keys = BTreeMap::new();
        for path in paths {
            let location = Location {
                url: format!("https://{path}.git"),
                alternate: None,
            };
            keys.insert(path.to_string(), (location, "g".to_string()));
        }
        name_repos(keys).into_iter().map(|repo| repo.name).collect()
    }

    #[test]
    fn qualifies_names_only_when_two_repositories_collide() {
        assert_eq!(
            names_of(&["h/a/api", "h/b/api", "h/b/web"]),
            ["a/api", "b/api", "web"]
        );
    }

    #[test]
    fn keeps_names_unique_however_deep_the_collision() {
        assert_eq!(
            names_of(&["h/g1/sub/api", "h/g2/sub/api"]),
            ["g1/sub/api", "g2/sub/api"]
        );
        assert_eq!(
            names_of(&["github.com/acme/api", "gitlab.com/acme/api"]),
            ["github.com/acme/api", "gitlab.com/acme/api"]
        );
    }

    #[test]
    fn rejects_a_local_path_sent_by_an_api() {
        let remote = Remote {
            name: "evil".into(),
            ssh: "git@h:a.git".into(),
            https: "/etc/secrets".into(),
        };
        assert!(checked_location(&remote, Protocol::Https, false).is_err());
        let fine = checked_location(&remote, Protocol::Ssh, false).unwrap();
        assert_eq!(fine.url, "git@h:a.git");
        let without_local = checked_location(&remote, Protocol::Auto, true).unwrap();
        assert_eq!(without_local.alternate, None);
    }
}
