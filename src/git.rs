use anyhow::{Result, bail};
use regex::Regex;
use std::path::Path;
use std::process::Command;

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };
const REPOSITORY_LOCATORS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

/// Removes passwords and tokens from a text about to be printed.
pub fn scrub(text: &str) -> String {
    Regex::new(r"(https?://)[^/@\s]+@")
        .expect("static pattern")
        .replace_all(text, "$1")
        .into_owned()
}

fn finish(mut command: Command, label: &[&str]) -> Result<String> {
    let output = command.output()?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            scrub(&label.join(" ")),
            scrub(String::from_utf8_lossy(&output.stderr).trim())
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn base(repo: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(repo) = repo {
        command.arg("-C").arg(repo);
    }
    command.args(["-c", "protocol.ext.allow=never"]);
    command
}

/// Reads a mirror with a neutral environment, so that no user setting can change the result.
pub fn read(repo: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = base(repo);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    let attributes = format!("core.attributesFile={NULL_DEVICE}");
    command
        .args(["-c", "core.quotepath=off", "-c", &attributes])
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .args(args);
    finish(command, args)
}

/// Talks to a remote with the user's configuration: credential helpers, `insteadOf` and `core.sshCommand` live there.
pub fn network(repo: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = base(repo);
    for name in REPOSITORY_LOCATORS {
        command.env_remove(name);
    }
    command.env("GIT_TERMINAL_PROMPT", "0").args(args);
    finish(command, args)
}

pub fn version() -> Result<String> {
    Ok(read(None, &["--version"])?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_credentials_anywhere_in_a_message() {
        assert_eq!(
            scrub("clone -- https://user:s3cret@host/a.git failed for https://tok@b/c"),
            "clone -- https://host/a.git failed for https://b/c"
        );
        assert_eq!(scrub("git@host:a.git"), "git@host:a.git");
    }
}
