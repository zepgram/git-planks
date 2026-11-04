use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("git-planks-test-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn repo(&self, name: &str) -> Repo {
        let path = self.root.join(name);
        fs::create_dir_all(&path).unwrap();
        let repo = Repo { path };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn planks(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_git-planks"))
            .args(args)
            .current_dir(&self.root)
            .env("GIT_PLANKS_CACHE", self.root.join("cache"))
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn analyze(&self, args: &[&str]) -> Data {
        let output = self.planks(&[&["analyze"], args].concat());
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Data::read(&self.root.join("planks-report"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Repo {
    path: PathBuf,
}

impl Repo {
    fn git(&self, args: &[&str]) {
        let done = Command::new("git")
            .args(args)
            .current_dir(&self.path)
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            done.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&done.stderr)
        );
    }

    fn write(&self, file: &str, lines: usize, prefix: &str) {
        let path = self.path.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content: String = (0..lines).map(|i| format!("{prefix} {i}\n")).collect();
        fs::write(path, content).unwrap();
    }

    fn commit(&self, author: (&str, &str), date: &str, subject: &str) {
        self.git(&["add", "-A"]);
        let stamp = format!("{date}T12:00:00Z");
        let done = Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                subject,
            ])
            .current_dir(&self.path)
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", author.0)
            .env("GIT_AUTHOR_EMAIL", author.1)
            .env("GIT_COMMITTER_NAME", author.0)
            .env("GIT_COMMITTER_EMAIL", author.1)
            .env("GIT_AUTHOR_DATE", &stamp)
            .env("GIT_COMMITTER_DATE", &stamp)
            .output()
            .unwrap();
        assert!(
            done.status.success(),
            "{}",
            String::from_utf8_lossy(&done.stderr)
        );
    }
}

/// An absolute path as git reads it: Windows' canonical `\\?\C:\` form is refused.
fn git_path(path: &Path) -> String {
    let absolute = path.canonicalize().unwrap();
    let text = absolute.to_string_lossy();
    text.trim_start_matches(r"\\?\").replace('\\', "/")
}

const ADA: (&str, &str) = ("Ada Lovelace", "ada@acme.dev");
const ALAN: (&str, &str) = ("Alan Turing", "alan@acme.dev");

struct Data {
    value: Value,
}

impl Data {
    fn read(directory: &Path) -> Self {
        let text = fs::read_to_string(directory.join("data.json")).unwrap();
        Self {
            value: serde_json::from_str(&text).unwrap(),
        }
    }

    fn date_index(&self, date: &str) -> usize {
        self.value["dates"]
            .as_array()
            .unwrap()
            .iter()
            .position(|d| d == date)
            .unwrap_or_else(|| panic!("no sample on {date}"))
    }

    fn lines_by_person(&self, date: &str) -> BTreeMap<String, i64> {
        let index = self.date_index(date) as i64;
        let mut totals = BTreeMap::new();
        for row in self.value["series"].as_array().unwrap() {
            let person = self.value["people"][row[1].as_u64().unwrap() as usize]["name"]
                .as_str()
                .unwrap()
                .to_string();
            let points: Vec<i64> = row[5]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_i64().unwrap())
                .collect();
            let current = points
                .as_chunks::<2>()
                .0
                .iter()
                .take_while(|pair| pair[0] <= index)
                .last()
                .map_or(0, |pair| pair[1]);
            *totals.entry(person).or_default() += current;
        }
        totals.retain(|_, lines: &mut i64| *lines != 0);
        totals
    }

    fn person_type(&self, name: &str) -> String {
        let people = self.value["people"].as_array().unwrap();
        people.iter().find(|p| p["name"] == name).unwrap()["type"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

fn expected(entries: &[(&str, i64)]) -> BTreeMap<String, i64> {
    entries
        .iter()
        .map(|(name, lines)| (name.to_string(), *lines))
        .collect()
}

fn two_author_repo(sandbox: &Sandbox) -> Repo {
    let repo = sandbox.repo("api");
    repo.write("src/a.rs", 10, "ada");
    repo.commit(ADA, "2024-01-03", "feat: start");
    repo.write("src/b.rs", 5, "alan");
    repo.commit(ALAN, "2024-01-10", "feat: add b");
    repo.write("src/a.rs", 4, "ada");
    repo.commit(ALAN, "2024-01-17", "refactor: shrink a");
    repo
}

#[test]
fn counts_only_the_lines_still_alive_at_each_date() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    let data = sandbox.analyze(&["api", "--until", "2024-01-22"]);
    assert_eq!(
        data.lines_by_person("2024-01-08"),
        expected(&[("Ada Lovelace", 10)])
    );
    assert_eq!(
        data.lines_by_person("2024-01-15"),
        expected(&[("Ada Lovelace", 10), ("Alan Turing", 5)])
    );
    assert_eq!(
        data.lines_by_person("2024-01-22"),
        expected(&[("Ada Lovelace", 4), ("Alan Turing", 5)])
    );
}

#[test]
fn stops_at_the_cutoff_even_when_the_branch_went_further() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    let data = sandbox.analyze(&["api", "--until", "2024-01-12"]);
    assert_eq!(
        data.lines_by_person("2024-01-12"),
        expected(&[("Ada Lovelace", 10), ("Alan Turing", 5)])
    );
}

#[test]
fn removes_a_deleted_file_from_the_curve() {
    let sandbox = Sandbox::new();
    let repo = two_author_repo(&sandbox);
    fs::remove_file(repo.path.join("src/b.rs")).unwrap();
    repo.commit(ADA, "2024-01-24", "chore: drop b");
    let data = sandbox.analyze(&["api", "--until", "2024-01-29"]);
    assert_eq!(
        data.lines_by_person("2024-01-29"),
        expected(&[("Ada Lovelace", 4)])
    );
}

#[test]
fn keeps_the_author_across_a_rename() {
    let sandbox = Sandbox::new();
    let repo = two_author_repo(&sandbox);
    repo.git(&["mv", "src/b.rs", "src/renamed.rs"]);
    repo.commit(ADA, "2024-01-24", "chore: rename");
    let data = sandbox.analyze(&["api", "--until", "2024-01-29"]);
    assert_eq!(
        data.lines_by_person("2024-01-29"),
        expected(&[("Ada Lovelace", 4), ("Alan Turing", 5)])
    );
}

#[test]
fn credits_branch_authors_through_a_merge() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("api");
    repo.write("a.rs", 3, "ada");
    repo.commit(ADA, "2024-01-03", "feat: start");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write("feature.rs", 7, "alan");
    repo.commit(ALAN, "2024-01-05", "feat: feature");
    repo.git(&["checkout", "-q", "main"]);
    repo.git(&[
        "-c",
        "user.name=Ada Lovelace",
        "-c",
        "user.email=ada@acme.dev",
        "merge",
        "-q",
        "--no-ff",
        "-m",
        "merge feature",
        "feature",
    ]);
    let data = sandbox.analyze(&["api", "--until", "2099-01-05", "--interval-weeks", "5200"]);
    let last = data.value["dates"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        data.lines_by_person(&last),
        expected(&[("Ada Lovelace", 3), ("Alan Turing", 7)])
    );
}

#[test]
fn ignores_lockfiles_generated_vendored_and_binary_files() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("api");
    repo.write("src/a.ts", 2, "code");
    repo.write("package-lock.json", 500, "lock");
    repo.write("vendor/lib.ts", 500, "vendored");
    repo.write("graphql/generated/ops.ts", 500, "generated");
    fs::write(repo.path.join("module.so"), [0u8, 1, 2, 255, 10, 0, 10]).unwrap();
    repo.commit(ADA, "2024-01-03", "feat: start");
    let data = sandbox.analyze(&["api", "--until", "2024-01-08"]);
    assert_eq!(
        data.lines_by_person("2024-01-08"),
        expected(&[("Ada Lovelace", 2)])
    );
}

#[test]
fn merges_identities_automatically_and_honors_the_mailmap() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("api");
    repo.write("a.rs", 2, "one");
    repo.commit(("ADA LOVELACE", "ada@old.dev"), "2024-01-03", "one");
    repo.write("b.rs", 3, "two");
    repo.commit(ADA, "2024-01-04", "two");
    repo.write("c.rs", 4, "three");
    repo.commit(("Grace", "grace@acme.dev"), "2024-01-05", "three");
    fs::write(
        repo.path.join(".mailmap"),
        "Grace Hopper <grace@acme.dev>\n",
    )
    .unwrap();
    repo.commit(ADA, "2024-01-06", "chore: mailmap");
    let data = sandbox.analyze(&["api", "--until", "2024-01-08"]);
    assert_eq!(
        data.lines_by_person("2024-01-08"),
        expected(&[("Ada Lovelace", 5), ("Grace Hopper", 4)])
    );
}

#[test]
fn classes_bots_and_agents_apart_from_people() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("api");
    repo.write("a.rs", 2, "one");
    repo.commit(
        (
            "dependabot[bot]",
            "49699333+dependabot[bot]@users.noreply.github.com",
        ),
        "2024-01-03",
        "bump",
    );
    repo.write("b.rs", 2, "two");
    repo.commit(("Claude", "noreply@anthropic.com"), "2024-01-04", "feat");
    let data = sandbox.analyze(&["api", "--until", "2024-01-08"]);
    assert_eq!(data.person_type("dependabot[bot]"), "bot");
    assert_eq!(data.person_type("Claude"), "agent");
}

#[test]
fn moves_a_bulk_import_out_of_personal_totals() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("api");
    repo.write("legacy.rs", 100, "old");
    repo.commit(ADA, "2024-01-03", "chore: import from the old forge");
    repo.write("new.rs", 5, "new");
    repo.commit(ADA, "2024-01-04", "feat: new");
    fs::write(
        sandbox.root.join("planks.toml"),
        "[repos.api]\nimport_subject = \"^chore: import\"\n",
    )
    .unwrap();
    let data = sandbox.analyze(&["api", "--until", "2024-01-08"]);
    assert_eq!(
        data.lines_by_person("2024-01-08"),
        expected(&[("Ada Lovelace", 5), ("Imports (unattributed)", 100)])
    );
    assert_eq!(data.person_type("Imports (unattributed)"), "import");
}

#[test]
fn groups_several_repositories_and_expands_a_list_file() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    let web = sandbox.repo("web");
    web.write("app.ts", 3, "web");
    web.commit(ALAN, "2024-01-03", "feat");
    fs::write(sandbox.root.join("repos.txt"), "# stack\nweb\n").unwrap();
    let data = sandbox.analyze(&["back=api", "front=@repos.txt", "--until", "2024-01-22"]);
    let repos = data.value["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0]["group"], "back");
    assert_eq!(repos[1]["group"], "front");
    assert_eq!(
        data.lines_by_person("2024-01-22"),
        expected(&[("Ada Lovelace", 4), ("Alan Turing", 8)])
    );
}

fn repo_with_submodule(sandbox: &Sandbox, address: &str) {
    let child = sandbox.repo("child");
    child.write("lib.rs", 6, "child");
    child.commit(ALAN, "2024-01-03", "feat");
    let parent = sandbox.repo("parent");
    parent.write("main.rs", 2, "parent");
    let address = if address.is_empty() {
        git_path(&child.path)
    } else {
        address.to_string()
    };
    parent.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        &address,
        "child",
    ]);
    parent.commit(ADA, "2024-01-04", "feat");
}

#[test]
fn follows_a_local_checkout_relative_submodule_only_through_its_remote() {
    let sandbox = Sandbox::new();
    repo_with_submodule(&sandbox, "../child");
    let data = sandbox.analyze(&["parent", "--submodules", "--until", "2024-01-08"]);
    assert_eq!(
        data.value["repos"].as_array().unwrap().len(),
        1,
        "a hostile checkout must not pull in its sibling folders"
    );
}

#[test]
fn refuses_an_absolute_local_submodule_address() {
    let sandbox = Sandbox::new();
    repo_with_submodule(&sandbox, "");
    let data = sandbox.analyze(&["parent", "--submodules", "--until", "2024-01-08"]);
    assert_eq!(data.value["repos"].as_array().unwrap().len(), 1);
}

#[test]
fn skips_an_empty_repository_and_names_it() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    sandbox.repo("empty");
    let data = sandbox.analyze(&["api", "empty", "--until", "2024-01-22"]);
    assert_eq!(data.value["manifest"]["skipped"][0], "empty");
    assert_eq!(data.value["repos"].as_array().unwrap().len(), 1);
}

#[test]
fn explains_an_unreachable_repository_and_keeps_going_on_request() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    let folder = sandbox.root.join("not-a-repo");
    fs::create_dir_all(&folder).unwrap();
    let failure = sandbox.planks(&["analyze", "api", "not-a-repo", "--until", "2024-01-22"]);
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("--keep-going"));
    let data = sandbox.analyze(&["api", "not-a-repo", "--until", "2024-01-22", "--keep-going"]);
    assert_eq!(data.value["manifest"]["skipped"][0], "not-a-repo");
}

#[test]
fn produces_identical_bytes_with_and_without_the_cache() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    let report = sandbox.root.join("planks-report/report.html");
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    let first = fs::read(&report).unwrap();
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    assert_eq!(first, fs::read(&report).unwrap());
    fs::remove_dir_all(sandbox.root.join("cache")).unwrap();
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    assert_eq!(first, fs::read(&report).unwrap());
}

#[test]
fn verify_proves_a_report_and_catches_a_tampered_one() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    let verified = sandbox.planks(&["verify"]);
    let text = String::from_utf8_lossy(&verified.stdout).into_owned();
    assert!(verified.status.success(), "{text}");
    assert!(text.contains("identical"), "{text}");
    assert!(text.contains("attributed          9"), "{text}");
    let report = sandbox.root.join("planks-report/report.html");
    let original = fs::read_to_string(&report).unwrap();
    fs::write(&report, original.replace("Code ownership", "Tampered")).unwrap();
    assert!(!sandbox.planks(&["verify"]).status.success());
}

#[test]
fn verify_replays_after_the_branch_moved_on() {
    let sandbox = Sandbox::new();
    let repo = two_author_repo(&sandbox);
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    repo.write("src/later.rs", 50, "later");
    repo.commit(ADA, "2024-01-23", "feat: later");
    let verified = sandbox.planks(&["verify"]);
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stdout)
    );
}

#[test]
fn audit_names_the_files_behind_a_jump() {
    let sandbox = Sandbox::new();
    let repo = two_author_repo(&sandbox);
    repo.write("src/huge_import.rs", 400, "bulk");
    repo.commit(ALAN, "2024-01-24", "chore: bulk");
    sandbox.analyze(&["api", "--until", "2024-01-29"]);
    let audit = sandbox.planks(&["audit"]);
    let text = String::from_utf8_lossy(&audit.stdout);
    assert!(
        audit.status.success(),
        "{}",
        String::from_utf8_lossy(&audit.stderr)
    );
    assert!(
        text.contains("+400") && text.contains("src/huge_import.rs"),
        "{text}"
    );
}

#[test]
fn escapes_hostile_text_in_the_report() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    sandbox.analyze(&[
        "api",
        "--until",
        "2024-01-22",
        "--title",
        "</script><script>alert(1)</script>",
    ]);
    let report = fs::read_to_string(sandbox.root.join("planks-report/report.html")).unwrap();
    assert!(!report.contains("<script>alert"));
    assert!(report.contains("\\u003cscript"));
}

#[test]
fn keeps_emails_out_of_the_shareable_report() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    let report = fs::read_to_string(sandbox.root.join("planks-report/report.html")).unwrap();
    assert!(!report.contains("ada@acme.dev"));
    assert!(
        fs::read_to_string(sandbox.root.join("planks-report/identities.json"))
            .unwrap()
            .contains("ada@acme.dev")
    );
}

#[test]
fn rejects_a_source_it_cannot_understand() {
    let sandbox = Sandbox::new();
    let output = sandbox.planks(&["analyze", "does-not-exist"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("github:owner"));
}

#[test]
fn analyzes_a_checkout_with_a_detached_head() {
    let sandbox = Sandbox::new();
    let repo = two_author_repo(&sandbox);
    repo.git(&["checkout", "-q", "--detach", "HEAD~1"]);
    let data = sandbox.analyze(&["api", "--until", "2024-01-22"]);
    assert_eq!(
        data.lines_by_person("2024-01-22"),
        expected(&[("Ada Lovelace", 10), ("Alan Turing", 5)])
    );
}

#[test]
fn refuses_a_shallow_clone_instead_of_misattributing_it() {
    let sandbox = Sandbox::new();
    let repo = two_author_repo(&sandbox);
    let path = git_path(&repo.path);
    let origin = if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    };
    let done = Command::new("git")
        .args(["clone", "-q", "--depth", "1", &origin, "shallow"])
        .current_dir(&sandbox.root)
        .output()
        .unwrap();
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
    let output = sandbox.planks(&["analyze", "shallow"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("shallow"));
}

#[test]
fn fails_instead_of_reporting_stale_numbers_when_a_remote_is_gone() {
    let sandbox = Sandbox::new();
    let repo = two_author_repo(&sandbox);
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    let path = repo.path.canonicalize().unwrap();
    fs::rename(path.join(".git"), path.join("moved")).unwrap();
    assert!(
        !sandbox
            .planks(&["analyze", "api", "--until", "2024-01-22"])
            .status
            .success()
    );
}

#[test]
fn follows_the_branch_the_remote_serves_as_head() {
    let sandbox = Sandbox::new();
    let repo = two_author_repo(&sandbox);
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    repo.git(&["checkout", "-q", "-b", "trunk"]);
    repo.write("src/trunk.rs", 50, "trunk");
    repo.commit(ADA, "2024-01-18", "feat: trunk");
    repo.git(&["branch", "-q", "-D", "main"]);
    let data = sandbox.analyze(&["api", "--until", "2024-01-22"]);
    assert_eq!(
        data.lines_by_person("2024-01-22"),
        expected(&[("Ada Lovelace", 54), ("Alan Turing", 5)])
    );
}

#[test]
fn keeps_two_repositories_with_similar_paths_apart() {
    let sandbox = Sandbox::new();
    for (group, lines) in [("g1", 100), ("g2", 7)] {
        let repo = sandbox.repo(&format!("{group}/sub/api"));
        repo.write("a.rs", lines, "x");
        repo.commit(ADA, "2024-01-03", "feat");
    }
    let data = sandbox.analyze(&["g1/sub/api", "g2/sub/api", "--until", "2024-01-08"]);
    assert_eq!(data.value["repos"].as_array().unwrap().len(), 2);
    assert_eq!(
        data.lines_by_person("2024-01-08"),
        expected(&[("Ada Lovelace", 107)])
    );
}

#[test]
fn warns_when_a_repository_rule_matches_nothing() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    fs::write(
        sandbox.root.join("planks.toml"),
        "[repos.apii]\nexclude = [\"x\"]\n",
    )
    .unwrap();
    let output = sandbox.planks(&["analyze", "api", "--until", "2024-01-22"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("[repos.apii] matches no repository"));
}

#[test]
fn refuses_a_config_that_would_make_git_write_a_file() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    let precious = sandbox.root.join("precious");
    fs::write(&precious, "keep me").unwrap();
    let config = format!(
        "[rules]\nblame_options = [\"--output={}\"]\n",
        precious.display()
    );
    fs::write(sandbox.root.join("planks.toml"), config).unwrap();
    assert!(
        !sandbox
            .planks(&["analyze", "api", "--until", "2024-01-22"])
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(&precious).unwrap(), "keep me");
}

#[test]
fn does_not_send_a_token_to_a_host_named_by_a_config_file() {
    let sandbox = Sandbox::new();
    fs::write(
        sandbox.root.join("planks.toml"),
        "[[source]]\nfrom = \"gitlab:https://attacker.invalid/x\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_git-planks"))
        .args(["repos"])
        .current_dir(&sandbox.root)
        .env("GITLAB_TOKEN", "secret")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("GITLAB_TOKEN is not sent to attacker.invalid"),
        "{stderr}"
    );
}

#[test]
fn does_not_print_credentials_of_a_clone_address() {
    let sandbox = Sandbox::new();
    let output = sandbox.planks(&["analyze", "https://user:s3cret@127.0.0.1:9/acme/api.git"]);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("s3cret"));
}

#[test]
fn verify_catches_a_poisoned_blame_cache_and_a_tampered_data_file() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    let data = sandbox.root.join("planks-report/data.json");
    let original = fs::read_to_string(&data).unwrap();
    fs::write(&data, original.replace("Alan Turing", "Someone Else")).unwrap();
    let verified = sandbox.planks(&["verify"]);
    assert!(!verified.status.success());
    assert!(String::from_utf8_lossy(&verified.stdout).contains("data.json: DIFFERENT"));
    fs::write(&data, &original).unwrap();

    let cache = sandbox.root.join("cache");
    let blame_dir = fs::read_dir(&cache)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("blame-"))
        .unwrap();
    let file = fs::read_dir(&blame_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let text = fs::read_to_string(&file).unwrap();
    let sha = serde_json::from_str::<Value>(text.lines().next().unwrap()).unwrap()[2]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    let poisoned: Vec<String> = text
        .lines()
        .map(|line| {
            let mut row: Value = serde_json::from_str(line).unwrap();
            let total: u64 = row[2]
                .as_object()
                .unwrap()
                .values()
                .map(|v| v.as_u64().unwrap())
                .sum();
            row[2] = serde_json::json!({ sha.clone(): total });
            row.to_string()
        })
        .collect();
    fs::write(&file, poisoned.join("\n") + "\n").unwrap();
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    assert!(
        !sandbox.planks(&["verify"]).status.success(),
        "a report built on a corrupt cache must not verify"
    );
    assert!(sandbox.planks(&["verify", "--cached"]).status.success());
}

#[test]
fn gives_the_same_report_when_only_later_commits_changed() {
    let sandbox = Sandbox::new();
    let repo = two_author_repo(&sandbox);
    let report = sandbox.root.join("planks-report/report.html");
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    let first = fs::read(&report).unwrap();
    fs::write(repo.path.join(".mailmap"), "Renamed Later <ada@acme.dev>\n").unwrap();
    repo.commit(ALAN, "2024-02-01", "chore: mailmap");
    sandbox.analyze(&["api", "--until", "2024-01-22"]);
    assert_eq!(first, fs::read(&report).unwrap());
}

#[test]
fn ignores_the_git_environment_of_a_hook() {
    let sandbox = Sandbox::new();
    two_author_repo(&sandbox);
    let output = Command::new(env!("CARGO_BIN_EXE_git-planks"))
        .args(["analyze", "api", "--until", "2024-01-22"])
        .current_dir(&sandbox.root)
        .env("GIT_PLANKS_CACHE", sandbox.root.join("cache"))
        .env("GIT_DIR", sandbox.root.join("nowhere"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "mailmap.file")
        .env("GIT_CONFIG_VALUE_0", "/does/not/exist")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
