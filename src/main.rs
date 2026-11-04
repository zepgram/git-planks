mod checks;
mod config;
mod dates;
mod engine;
mod git;
mod identities;
mod output;
mod rules;
mod sources;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use config::Config;
use engine::{Manifest, ManifestRepo, Skipped};
use sources::{Discovery, Protocol, Repo, Requested};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const SOURCE_HELP: &str = "Repositories to analyze, each optionally prefixed with `group=`: \
    github:owner, gitlab:group/subgroup, bitbucket:workspace, \
    self-hosted gitlab:https://git.acme.dev/team or github:https://ghe.acme.dev/org, git@host:org/repo.git, https://host/org/repo.git, a local folder, or @file listing one per line";

#[derive(Parser)]
#[command(
    name = "git-planks",
    version,
    about = "Who owns the code that is still alive? Deterministic surviving-lines analysis of a whole stack."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Args)]
struct AnalyzeArgs {
    #[arg(help = SOURCE_HELP)]
    sources: Vec<String>,
    /// Config file (default: ./planks.toml when present)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Report title
    #[arg(long)]
    title: Option<String>,
    /// Last day analyzed, YYYY-MM-DD (default: today, UTC)
    #[arg(long)]
    until: Option<String>,
    /// Weeks between two samples: raise it for very large histories
    #[arg(long, default_value_t = 1)]
    interval_weeks: i64,
    /// Output directory
    #[arg(long, default_value = "planks-report")]
    out: PathBuf,
    /// Parallel blames (default: number of CPUs)
    #[arg(long)]
    jobs: Option<usize>,
    /// Clone protocol. auto: SSH when an API token is set, HTTPS otherwise
    #[arg(long, value_enum, default_value_t = Protocol::Auto)]
    protocol: Protocol,
    /// Also analyze the submodules of each repository
    #[arg(long)]
    submodules: bool,
    /// Self-hosted instance allowed to receive GITHUB_TOKEN or GITLAB_TOKEN (repeatable)
    #[arg(long = "trust-host", value_name = "HOST")]
    trusted_hosts: Vec<String>,
    /// Skip a repository that cannot be cloned instead of stopping, and name it in the report
    #[arg(long)]
    keep_going: bool,
    /// Do not overlay AI model releases
    #[arg(long)]
    no_model_events: bool,
    /// Open the report when done
    #[arg(long)]
    open: bool,
    /// Print the summary as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Analyze repositories and write an interactive report
    Analyze(AnalyzeArgs),
    /// List the repositories a set of sources resolves to, without analyzing them
    Repos {
        #[arg(help = SOURCE_HELP)]
        sources: Vec<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = Protocol::Auto)]
        protocol: Protocol,
        #[arg(long)]
        submodules: bool,
        #[arg(long = "trust-host", value_name = "HOST")]
        trusted_hosts: Vec<String>,
    },
    /// Show the biggest jumps and largest files, to catch imports and generated code
    Audit {
        #[arg(default_value = "planks-report")]
        directory: PathBuf,
        #[arg(long, default_value_t = 15)]
        top: usize,
    },
    /// Replay an analysis from its manifest, blaming everything again, and prove the result identical
    Verify {
        #[arg(default_value = "planks-report")]
        directory: PathBuf,
        /// Reuse cached blame: faster, but it does not check the cache itself
        #[arg(long)]
        cached: bool,
    },
}

fn requested(sources: &[String], config: &Config, submodules: bool) -> Result<Vec<Requested>> {
    let mut list: Vec<Requested> = config.source.iter().map(Requested::from_config).collect();
    list.extend(
        sources
            .iter()
            .map(|source| Requested::from_argument(source, submodules)),
    );
    if list.is_empty() {
        bail!(
            "no source: pass one, for example `git-planks analyze github:owner`, or add a [[source]] to planks.toml"
        );
    }
    Ok(list)
}

fn pin_repositories(
    repos: Vec<Repo>,
    keep_going: bool,
) -> Result<(Vec<ManifestRepo>, Vec<Skipped>)> {
    let mut pinned = Vec::new();
    let mut skipped = Vec::new();
    for repo in repos {
        eprintln!("sync {}", output::printable(&repo.name));
        let url = sources::without_credentials(&repo.url);
        let skip = |reason: String| Skipped {
            name: repo.name.clone(),
            url: url.clone(),
            reason,
        };
        match sources::sync_repo(&repo.url, repo.alternate.as_deref()) {
            Ok((used, _, Some((branch, tip)))) => pinned.push(ManifestRepo {
                alternate: [Some(repo.url.as_str()), repo.alternate.as_deref()]
                    .into_iter()
                    .flatten()
                    .find(|other| *other != used)
                    .map(sources::without_credentials),
                url: sources::without_credentials(&used),
                name: repo.name,
                group: repo.group,
                branch,
                tip,
                sampled: None,
            }),
            Ok((_, _, None)) => skipped.push(skip("empty repository".to_string())),
            Err(error) if keep_going => {
                eprintln!("warning: {}: {error:#}", output::printable(&repo.name));
                skipped.push(skip("cannot be cloned".to_string()));
            }
            Err(error) => {
                bail!(
                    "{}: {error:#}\nCheck your access, or pass --keep-going to skip it.",
                    output::printable(&repo.name)
                );
            }
        }
    }
    if pinned.is_empty() {
        bail!("no repository with commits to analyze");
    }
    Ok((pinned, skipped))
}

fn open_in_browser(path: &Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(error) = Command::new(opener).arg(path).status() {
        eprintln!("warning: cannot open the report: {error}");
    }
}

fn print_summary(outcome: &engine::Outcome, written: &output::Written, directory: &Path) {
    let summary = &outcome.summary;
    println!(
        "{} surviving lines in {} repositories",
        summary["surviving_lines"],
        outcome.manifest.repos.len()
    );
    for row in summary["top_people"].as_array().into_iter().flatten() {
        println!(
            "  {:>10}  {}",
            row["lines"].as_i64().unwrap_or(0),
            output::printable(row["name"].as_str().unwrap_or(""))
        );
    }
    if outcome.merged_identities > 0 {
        println!(
            "{} people had several git identities merged: review {}",
            outcome.merged_identities,
            directory.join("identities.json").display()
        );
    }
    for skipped in &outcome.manifest.skipped {
        println!(
            "skipped {}: {}",
            output::printable(&skipped.name),
            skipped.reason
        );
    }
    println!("report {}", written.report.display());
    println!("sha256 {}", output::digest(&written.rendered.report));
}

fn run_analyze(args: AnalyzeArgs) -> Result<()> {
    if args.interval_weeks < 1 {
        bail!("--interval-weeks must be at least 1");
    }
    if let Some(jobs) = args.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global()?;
    }
    let mut config = Config::load(args.config.as_deref())?;
    if args.title.is_some() {
        config.title = args.title;
    }
    if args.no_model_events {
        config.model_events = Some(false);
    }
    let until = args
        .until
        .unwrap_or_else(|| dates::format_date(dates::today()));
    dates::parse_date(&until)?;
    let list = requested(&args.sources, &config, args.submodules)?;
    let discovery = Discovery {
        protocol: args.protocol,
        keep_going: args.keep_going,
        trusted_hosts: &args.trusted_hosts,
    };
    let repos = sources::discover(&list, &discovery)?;
    for name in config.repos.keys() {
        if !repos.iter().any(|repo| repo.name == *name) {
            eprintln!(
                "warning: [repos.{name}] matches no repository: see `git-planks repos` for their names"
            );
        }
    }
    let (pinned, skipped) = pin_repositories(repos, args.keep_going)?;
    let manifest = Manifest {
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        git: git::version()?,
        until,
        interval_weeks: args.interval_weeks,
        config,
        repos: pinned,
        skipped,
    };
    let outcome = engine::analyze(manifest, false)?;
    let written = output::write(&args.out, &outcome)?;
    if args.json {
        let summary = serde_json::json!({
            "report": written.report,
            "report_sha256": output::digest(&written.rendered.report),
            "data_sha256": output::digest(&written.rendered.payload),
            "merged_identities": outcome.merged_identities,
            "skipped": outcome.manifest.skipped,
            "summary": outcome.summary,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_summary(&outcome, &written, &args.out);
    }
    if args.open {
        open_in_browser(&written.report);
    }
    Ok(())
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Commands::Analyze(args) => run_analyze(args),
        Commands::Repos {
            sources,
            config,
            protocol,
            submodules,
            trusted_hosts,
        } => {
            let config = Config::load(config.as_deref())?;
            let list = requested(&sources, &config, submodules)?;
            let discovery = Discovery {
                protocol,
                keep_going: false,
                trusted_hosts: &trusted_hosts,
            };
            for repo in sources::discover(&list, &discovery)? {
                let line = format!(
                    "{}\t{}\t{}",
                    repo.group,
                    repo.name,
                    sources::without_credentials(&repo.url)
                );
                println!(
                    "{}",
                    line.split('\t')
                        .map(output::printable)
                        .collect::<Vec<_>>()
                        .join("\t")
                );
            }
            Ok(())
        }
        Commands::Audit { directory, top } => checks::audit(&directory, top),
        Commands::Verify { directory, cached } => checks::verify(&directory, cached),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
