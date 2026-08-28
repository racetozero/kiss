//! Prompt templates: Markdown files whose name becomes a `/command`, with
//! positional-argument expansion.

use crate::skills::split_frontmatter;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub body: String,
    pub path: PathBuf,
}

fn parse_template(path: &Path) -> Option<PromptTemplate> {
    let text = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = split_frontmatter(&text);
    let name = path.file_stem()?.to_str()?.to_string();
    let mut description = None;
    let mut argument_hint = None;
    if let Some(fm) = frontmatter
        && let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(fm)
    {
        description = value["description"].as_str().map(String::from);
        argument_hint = value["argument-hint"].as_str().map(String::from);
    }
    let description = description
        .or_else(|| {
            body.lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
        })
        .unwrap_or_default();
    Some(PromptTemplate {
        name,
        description,
        argument_hint,
        body: body.to_string(),
        path: path.to_path_buf(),
    })
}

fn scan(dir: &Path, out: &mut Vec<PromptTemplate>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md")
            && let Some(t) = parse_template(&path)
        {
            out.push(t);
        }
    }
}

pub fn discover(cwd: &Path, project_trusted: bool, extra_paths: &[PathBuf]) -> Vec<PromptTemplate> {
    let mut out = Vec::new();
    if let Some(home) = dirs::home_dir() {
        scan(&home.join(".kiss/agent/prompts"), &mut out);
    }
    if project_trusted {
        scan(&cwd.join(".kiss/prompts"), &mut out);
    }
    for path in extra_paths {
        if path.is_dir() {
            scan(path, &mut out);
        } else if let Some(t) = parse_template(path) {
            out.push(t);
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|t| seen.insert(t.name.clone()));
    out
}

/// Expand `$1..$n`, `$@`/`$ARGUMENTS`, and `${n:-default}` in the body.
pub fn expand(body: &str, args: &[&str]) -> String {
    let joined = args.join(" ");
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('@') => {
                chars.next();
                out.push_str(&joined);
            }
            Some('{') => {
                chars.next();
                let mut inner = String::new();
                for ic in chars.by_ref() {
                    if ic == '}' {
                        break;
                    }
                    inner.push(ic);
                }
                // ${n:-default}
                let (num, default) = match inner.split_once(":-") {
                    Some((n, d)) => (n, Some(d)),
                    None => (inner.as_str(), None),
                };
                if let Ok(index) = num.parse::<usize>() {
                    match args.get(index.wrapping_sub(1)) {
                        Some(v) if !v.is_empty() => out.push_str(v),
                        _ => out.push_str(default.unwrap_or("")),
                    }
                } else {
                    out.push_str("${");
                    out.push_str(&inner);
                    out.push('}');
                }
            }
            Some(d) if d.is_ascii_digit() => {
                let mut num = String::new();
                while let Some(d) = chars.peek() {
                    if d.is_ascii_digit() {
                        num.push(*d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let index: usize = num.parse().unwrap_or(0);
                if let Some(v) = args.get(index.wrapping_sub(1)) {
                    out.push_str(v);
                }
            }
            Some('A') => {
                // $ARGUMENTS
                let rest: String = chars.clone().take(9).collect();
                if rest == "ARGUMENTS" {
                    for _ in 0..9 {
                        chars.next();
                    }
                    out.push_str(&joined);
                } else {
                    out.push('$');
                }
            }
            _ => out.push('$'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion() {
        assert_eq!(
            expand("fix $1 in $2", &["bug", "file.rs"]),
            "fix bug in file.rs"
        );
        assert_eq!(expand("all: $@", &["a", "b"]), "all: a b");
        assert_eq!(expand("all: $ARGUMENTS", &["a", "b"]), "all: a b");
        assert_eq!(expand("x ${1:-default}", &[]), "x default");
        assert_eq!(expand("x ${1:-default}", &["given"]), "x given");
        assert_eq!(expand("$3 missing", &["a"]), " missing");
    }

    #[test]
    fn template_parse_uses_first_line_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.md");
        std::fs::write(&path, "Review the staged changes carefully.\nMore text.").unwrap();
        let t = parse_template(&path).unwrap();
        assert_eq!(t.name, "review");
        assert_eq!(t.description, "Review the staged changes carefully.");
    }
}
