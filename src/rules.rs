use crate::config::Config;
use anyhow::{Context, Result};
use regex::{Regex, RegexSet};
use std::collections::{BTreeMap, HashMap};

const LANGUAGES: &[(&str, &str)] = &[
    ("TypeScript", "ts tsx mts cts"),
    ("JavaScript", "js jsx mjs cjs"),
    ("Vue", "vue"),
    ("Svelte", "svelte"),
    ("Python", "py pyi pyx"),
    ("Go", "go"),
    ("Rust", "rs"),
    ("Java", "java"),
    ("Kotlin", "kt kts"),
    ("Scala", "scala sc"),
    ("Groovy", "groovy gradle"),
    ("C", "c h"),
    ("C++", "cc cpp cxx hh hpp hxx"),
    ("C#", "cs csx"),
    ("F#", "fs fsx"),
    ("Swift", "swift"),
    ("Objective-C", "m mm"),
    ("Dart", "dart"),
    ("Ruby", "rb rake gemspec erb"),
    ("PHP", "php phtml"),
    ("Elixir", "ex exs"),
    ("Erlang", "erl hrl"),
    ("Clojure", "clj cljs cljc edn"),
    ("Haskell", "hs"),
    ("OCaml", "ml mli"),
    ("Lua", "lua"),
    ("Perl", "pl pm"),
    ("R", "r"),
    ("Julia", "jl"),
    ("Zig", "zig"),
    ("Nim", "nim"),
    ("Solidity", "sol"),
    ("Apex", "cls trigger"),
    ("DataWeave", "dwl"),
    ("PowerShell", "ps1 psm1"),
    ("Shell", "sh bash zsh fish"),
    ("SQL", "sql"),
    ("GraphQL", "graphql graphqls gql"),
    ("Protobuf", "proto"),
    ("Terraform", "tf tfvars hcl"),
    ("Nix", "nix"),
    ("HTML", "html htm astro twig jinja j2 hbs ejs tpl gotmpl"),
    ("CSS", "css scss sass less styl"),
    ("YAML", "yaml yml"),
    ("JSON", "json jsonc json5"),
    ("XML", "xml"),
    ("TOML", "toml"),
    ("INI", "ini cfg conf"),
    ("RAML", "raml"),
    ("CSV", "csv tsv"),
    ("Markdown", "md mdx rst adoc"),
];
const FILENAMES: &[(&str, &str)] = &[
    ("Dockerfile", "Shell"),
    ("Containerfile", "Shell"),
    ("Makefile", "Shell"),
    ("Justfile", "Shell"),
    ("Rakefile", "Ruby"),
    ("Gemfile", "Ruby"),
    ("Jenkinsfile", "Groovy"),
];
const CONFIG_LANGUAGES: &[&str] = &["YAML", "JSON", "XML", "TOML", "INI"];
const DEFAULT_EXCLUDE: &[&str] = &[
    "**/package-lock.json",
    "**/npm-shrinkwrap.json",
    "**/pnpm-lock.yaml",
    "**/yarn.lock",
    "**/bun.lock",
    "**/composer.lock",
    "**/Gemfile.lock",
    "**/Cargo.lock",
    "**/go.sum",
    "**/poetry.lock",
    "**/uv.lock",
    "**/pdm.lock",
    "**/Pipfile.lock",
    "**/flake.lock",
    "**/mix.lock",
    "**/pubspec.lock",
    "**/Podfile.lock",
    "**/gradle.lockfile",
    "**/packages.lock.json",
    "**/.terraform.lock.hcl",
    "**/node_modules/**",
    "**/vendor/**",
    "**/third_party/**",
    "**/thirdparty/**",
    "**/Pods/**",
    "**/bower_components/**",
    "**/dist/**",
    "**/build/**",
    "**/target/**",
    "**/.next/**",
    "**/.nuxt/**",
    "**/.output/**",
    "**/coverage/**",
    "**/*.min.js",
    "**/*.min.css",
    "**/*.map",
    "**/*.snap",
    "**/__snapshots__/**",
    "**/generated/**",
    "**/__generated__/**",
    "**/*.generated.*",
    "**/*.gen.*",
    "**/*.g.dart",
    "**/*.pb.go",
    "**/*_pb2.py",
    "**/*_pb2_grpc.py",
    "**/drizzle/meta/**",
    "**/migrations/meta/**",
];
const DEFAULT_KIND_RULES: &[(&str, &str)] = &[
    (
        "agents",
        r"(^|/)\.(claude|cursor|aider|codex|windsurf)/|(^|/)(CLAUDE|AGENTS|GEMINI)\.md$|(^|/)\.cursorrules$|(^|/)\.github/copilot-instructions\.md$",
    ),
    ("docs", r"\.(md|mdx|rst|adoc)$|(^|/)docs?/"),
    (
        "data",
        r"(^|/)(i18n|locales?|lang|fixtures|__fixtures__|testdata)(/|$)|\.(csv|tsv)$",
    ),
    (
        "tests",
        r"(^|/)(tests?|__tests__|e2e|specs?)(/|$)|\.(spec|test)\.[a-z]+$|(^|/)test_[^/]+$|_test\.[a-z]+$|Tests?\.[a-z]+$",
    ),
];
pub const KINDS: &[(&str, &str)] = &[
    ("code", "Code"),
    ("tests", "Tests"),
    ("config", "Config"),
    ("data", "Data, i18n and fixtures"),
    ("docs", "Documentation"),
    ("agents", "Agent harness"),
];
pub const DEFAULT_KINDS: &[&str] = &["code", "tests", "config", "agents"];

pub fn glob_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    let mut rest = pattern;
    while let Some(next) = rest.chars().next() {
        if let Some(tail) = rest.strip_prefix("**/") {
            out.push_str("(?:.*/)?");
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("**") {
            out.push_str(".*");
            rest = tail;
        } else {
            match next {
                '*' => out.push_str("[^/]*"),
                '?' => out.push_str("[^/]"),
                other => out.push_str(&regex::escape(&other.to_string())),
            }
            rest = &rest[next.len_utf8()..];
        }
    }
    out.push('$');
    out
}

fn glob_set(patterns: impl IntoIterator<Item = String>) -> Result<RegexSet> {
    let compiled: Vec<String> = patterns.into_iter().map(|p| glob_regex(&p)).collect();
    RegexSet::new(compiled).context("invalid path pattern")
}

struct RepoSets {
    include: Option<RegexSet>,
    exclude: RegexSet,
}

pub struct PathRules {
    exclude: RegexSet,
    repos: HashMap<String, RepoSets>,
    kinds: Vec<(String, Regex)>,
    extensions: BTreeMap<String, String>,
    memo: HashMap<String, HashMap<String, Option<(String, String)>>>,
}

impl PathRules {
    pub fn new(config: &Config) -> Result<Self> {
        let exclude = glob_set(
            DEFAULT_EXCLUDE
                .iter()
                .map(ToString::to_string)
                .chain(config.rules.exclude.iter().cloned()),
        )?;
        let mut repos = HashMap::new();
        for (name, rules) in &config.repos {
            let include = if rules.include.is_empty() {
                None
            } else {
                Some(glob_set(rules.include.iter().cloned())?)
            };
            repos.insert(
                name.clone(),
                RepoSets {
                    include,
                    exclude: glob_set(rules.exclude.iter().cloned())?,
                },
            );
        }
        let mut kinds = Vec::new();
        for (kind, pattern) in &config.rules.kind_rules {
            let regex =
                Regex::new(pattern).with_context(|| format!("invalid kind rule `{pattern}`"))?;
            kinds.push((kind.clone(), regex));
        }
        for (kind, pattern) in DEFAULT_KIND_RULES {
            kinds.push((kind.to_string(), Regex::new(pattern)?));
        }
        let mut extensions = BTreeMap::new();
        for (language, list) in LANGUAGES {
            for extension in list.split_whitespace() {
                extensions.insert(extension.to_string(), language.to_string());
            }
        }
        for (extension, language) in &config.rules.languages {
            extensions.insert(extension.to_lowercase(), language.clone());
        }
        Ok(Self {
            exclude,
            repos,
            kinds,
            extensions,
            memo: HashMap::new(),
        })
    }

    pub fn classify(&mut self, repo: &str, path: &str) -> Option<(String, String)> {
        if let Some(found) = self.memo.get(repo).and_then(|paths| paths.get(path)) {
            return found.clone();
        }
        let found = self.compute(repo, path);
        self.memo
            .entry(repo.to_string())
            .or_default()
            .insert(path.to_string(), found.clone());
        found
    }

    fn compute(&self, repo: &str, path: &str) -> Option<(String, String)> {
        if let Some(sets) = self.repos.get(repo) {
            if sets.include.as_ref().is_some_and(|set| !set.is_match(path)) {
                return None;
            }
            if sets.exclude.is_match(path) {
                return None;
            }
        }
        if self.exclude.is_match(path) {
            return None;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        let language = FILENAMES
            .iter()
            .find(|(filename, _)| *filename == name)
            .map(|(_, language)| language.to_string())
            .or_else(|| {
                let extension = name.rsplit_once('.')?.1.to_lowercase();
                self.extensions.get(&extension).cloned()
            })?;
        let kind = self
            .kinds
            .iter()
            .find(|(_, rule)| rule.is_match(path))
            .map(|(kind, _)| kind.clone())
            .unwrap_or_else(|| {
                if CONFIG_LANGUAGES.contains(&language.as_str()) {
                    "config".to_string()
                } else {
                    "code".to_string()
                }
            });
        Some((language, kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoRules;

    fn rules() -> PathRules {
        PathRules::new(&Config::default()).unwrap()
    }

    fn kind_of(path: &str) -> Option<String> {
        rules().classify("repo", path).map(|(_, kind)| kind)
    }

    #[test]
    fn glob_star_stays_inside_a_directory() {
        let regex = Regex::new(&glob_regex("src/*.rs")).unwrap();
        assert!(regex.is_match("src/main.rs"));
        assert!(!regex.is_match("src/nested/main.rs"));
    }

    #[test]
    fn double_star_crosses_directories_and_matches_the_root() {
        let regex = Regex::new(&glob_regex("**/vendor/**")).unwrap();
        assert!(regex.is_match("vendor/a.js"));
        assert!(regex.is_match("a/b/vendor/c/d.js"));
        assert!(!regex.is_match("vendors/a.js"));
    }

    #[test]
    fn skips_lockfiles_vendored_generated_and_binary_files() {
        for path in [
            "pnpm-lock.yaml",
            "api/Cargo.lock",
            "vendor/lib.go",
            "web/dist/app.js",
            "graphql/generated/ops.ts",
            "app.min.js",
            "drizzle/meta/0001_snapshot.json",
            "logo.png",
            "module.so",
        ] {
            assert_eq!(kind_of(path), None, "{path}");
        }
    }

    #[test]
    fn classifies_what_it_counts() {
        assert_eq!(kind_of("src/main.rs").as_deref(), Some("code"));
        assert_eq!(kind_of("Dockerfile").as_deref(), Some("code"));
        assert_eq!(kind_of("src/cart.spec.ts").as_deref(), Some("tests"));
        assert_eq!(kind_of("tests/e2e.rs").as_deref(), Some("tests"));
        assert_eq!(kind_of("chart/values.yaml").as_deref(), Some("config"));
        assert_eq!(kind_of("README.md").as_deref(), Some("docs"));
        assert_eq!(kind_of("docs/data.json").as_deref(), Some("docs"));
        assert_eq!(kind_of("i18n/fr.json").as_deref(), Some("data"));
        assert_eq!(
            kind_of(".claude/skills/a/SKILL.md").as_deref(),
            Some("agents")
        );
        assert_eq!(kind_of("CLAUDE.md").as_deref(), Some("agents"));
    }

    #[test]
    fn repository_rules_narrow_what_counts() {
        let mut config = Config::default();
        config.repos.insert(
            "shop".to_string(),
            RepoRules {
                include: vec!["app/code/Acme/**".to_string()],
                exclude: vec!["**/legacy/**".to_string()],
                import_subject: None,
            },
        );
        let mut rules = PathRules::new(&config).unwrap();
        assert!(rules.classify("shop", "app/code/Acme/A.php").is_some());
        assert!(rules.classify("shop", "app/code/Magento/A.php").is_none());
        assert!(
            rules
                .classify("shop", "app/code/Acme/legacy/A.php")
                .is_none()
        );
        assert!(rules.classify("other", "app/code/Magento/A.php").is_some());
    }

    #[test]
    fn custom_kind_rules_win_over_defaults() {
        let mut config = Config::default();
        config
            .rules
            .kind_rules
            .push(("code".to_string(), r"\.flow\.xml$".to_string()));
        let mut rules = PathRules::new(&config).unwrap();
        assert_eq!(rules.classify("r", "a.flow.xml").unwrap().1, "code");
        assert_eq!(rules.classify("r", "a.xml").unwrap().1, "config");
    }
}
