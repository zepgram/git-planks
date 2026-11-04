use crate::engine::{Manifest, Outcome};
use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("../assets/report.html");

pub struct Rendered {
    pub payload: String,
    pub report: String,
}

/// Strips control characters: author names and paths must not drive the terminal.
pub fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

pub fn identities_text(identities: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(identities)? + "\n")
}

pub fn digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn render(data: &Value) -> Result<Rendered> {
    let payload = serde_json::to_string(data)?;
    // Author names are untrusted: no `<` may reach the inline script as markup.
    let embedded = payload.replace('<', "\\u003c");
    Ok(Rendered {
        report: TEMPLATE.replace("/*__DATA__*/null", &embedded),
        payload,
    })
}

pub struct Written {
    pub report: PathBuf,
    pub rendered: Rendered,
}

pub fn write(directory: &Path, outcome: &Outcome) -> Result<Written> {
    fs::create_dir_all(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    let rendered = render(&outcome.data)?;
    let report = directory.join("report.html");
    fs::write(&report, &rendered.report)?;
    fs::write(directory.join("data.json"), &rendered.payload)?;
    fs::write(
        directory.join("manifest.json"),
        serde_json::to_string_pretty(&outcome.manifest)? + "\n",
    )?;
    fs::write(
        directory.join("identities.json"),
        identities_text(&outcome.identities)?,
    )?;
    Ok(Written { report, rendered })
}

pub fn read_manifest(directory: &Path) -> Result<Manifest> {
    let path = directory.join("manifest.json");
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "cannot read {}: run `git-planks analyze` first",
            path.display()
        )
    })?;
    let manifest: Manifest = serde_json::from_str(&text)
        .with_context(|| format!("invalid manifest {}", path.display()))?;
    manifest
        .validate()
        .with_context(|| format!("invalid manifest {}", path.display()))?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_terminal_escapes() {
        assert_eq!(
            printable("Ada\u{1b}]0;PWNED\u{7} Lovelace"),
            "Ada]0;PWNED Lovelace"
        );
    }

    #[test]
    fn neutralizes_markup_in_author_names() {
        let rendered =
            render(&json!({"people": [{"name": "</script><script>alert(1)</script>"}]})).unwrap();
        assert!(!rendered.report.contains("</script><script>alert"));
        assert!(rendered.report.contains("\\u003c/script"));
    }

    #[test]
    fn renders_the_same_bytes_for_the_same_data() {
        let data = json!({"b": 1, "a": [1, 2]});
        assert_eq!(
            digest(&render(&data).unwrap().report),
            digest(&render(&data).unwrap().report)
        );
    }
}
