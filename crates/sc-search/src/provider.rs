//! Pluggable result sources below the app grid. Apps themselves stay built in
//! (frecency, drag-to-home); a provider is anything else that answers a query.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Hit {
    pub title: String,
    pub subtitle: String,
    pub path: PathBuf,
}

pub trait Provider: Send + Sync {
    fn title(&self) -> &str;
    fn search(&self, query: &str, limit: usize) -> Vec<Hit>;
}

/// Every provider whose backing tool is installed.
pub fn detect() -> Vec<Box<dyn Provider>> {
    let mut out: Vec<Box<dyn Provider>> = Vec::new();
    if on_path("dsearch") {
        out.push(Box::new(Dsearch));
    }
    out
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// danksearch: `dsearch search --json` against its running index.
pub struct Dsearch;

impl Provider for Dsearch {
    fn title(&self) -> &str {
        "Files"
    }

    fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let out = Command::new("dsearch")
            .args(["search", "--json", "-n", &limit.to_string(), query])
            .output();
        let Ok(out) = out else { return Vec::new() };
        parse_dsearch(&out.stdout)
    }
}

fn parse_dsearch(json: &[u8]) -> Vec<Hit> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(json) else {
        return Vec::new();
    };
    v["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| h["id"].as_str())
        .map(|p| file_hit(Path::new(p)))
        .collect()
}

fn file_hit(path: &Path) -> Hit {
    let home = std::env::var("HOME").unwrap_or_default();
    let parent = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let subtitle = match parent.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => parent,
    };
    Hit {
        title: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        subtitle,
        path: path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dsearch_hits() {
        let json = br#"{"status":{"total":2},"hits":[{"id":"/a/b/c.txt","score":0.9},{"id":"/x","score":0.1}]}"#;
        let hits = parse_dsearch(json);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "c.txt");
        assert_eq!(hits[0].path, Path::new("/a/b/c.txt"));
        assert!(parse_dsearch(b"garbage").is_empty());
    }
}
