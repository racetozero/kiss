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

fn parse_template(path: &Path, problems: &mut Vec<String>) -> Option<PromptTemplate> {
    let text = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = split_frontmatter(&text);
    let name = path.file_stem()?.to_str()?.to_string();
    let mut description = None;
    let mut argument_hint = None;
    if text.starts_with("---") && frontmatter.is_none() {
        problems.push(format!(
            "kiss ignores the prompt template {} because its frontmatter has no closing ---. Add the closing --- line.",
            path.display()
        ));
        return None;
    }
    if let Some(fm) = frontmatter {
        let value = match serde_yaml::from_str::<serde_yaml::Value>(fm) {
            Ok(value) => value,
            Err(error) => {
                problems.push(format!(
                    "kiss ignores the prompt template {} because its frontmatter is not valid YAML ({error}). Correct the frontmatter.",
                    path.display()
                ));
                return None;
            }
        };
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

fn scan(dir: &Path, out: &mut Vec<PromptTemplate>, problems: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md")
            && let Some(t) = parse_template(&path, problems)
        {
            out.push(t);
        }
    }
}

/// The prompt templates, and the files skipped because they could not be
/// read, for the caller to show.
pub fn discover(
    cwd: &Path,
    project_trusted: bool,
    extra_paths: &[PathBuf],
) -> (Vec<PromptTemplate>, Vec<String>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    if let Some(agent_dir) = kiss_ai::paths::agent_dir() {
        scan(&agent_dir.join("prompts"), &mut out, &mut problems);
    }
    if project_trusted {
        scan(&cwd.join(".kiss/prompts"), &mut out, &mut problems);
    }
    for path in extra_paths {
        if path.is_dir() {
            scan(path, &mut out, &mut problems);
        } else if let Some(t) = parse_template(path, &mut problems) {
            out.push(t);
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|t| seen.insert(t.name.clone()));
    (out, problems)
}

/// Expand `$1..$n`, `$@`/`$ARGUMENTS`, and `${n:-default}` in the body.
/// Split command arguments on whitespace. Single or double quotes group
/// words, and an empty quoted string (`""` or `''`) is kept as an argument.
pub fn parse_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut quote: Option<char> = None;
    for c in input.chars() {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                has_token = true;
            }
            None if c.is_whitespace() => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        args.push(current);
    }
    args
}

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
    fn quoted_arguments_group_words_and_keep_empty_values() {
        assert_eq!(
            parse_args(r#"one "two words" '' 'x' "" last"#),
            ["one", "two words", "", "x", "", "last"]
        );
        let args = parse_args(r#""" b"#);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(expand("${1:-first} $2", &args), "first b");
    }

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
    fn malformed_frontmatter_rejects_the_template() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.md");
        std::fs::write(&path, "---\ndescription: [broken\n---\nbody").unwrap();
        let mut problems = Vec::new();
        assert!(parse_template(&path, &mut problems).is_none());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("broken.md"), "{problems:?}");
    }

    #[test]
    fn template_parse_uses_first_line_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.md");
        std::fs::write(&path, "Review the staged changes carefully.\nMore text.").unwrap();
        let t = parse_template(&path, &mut Vec::new()).unwrap();
        assert_eq!(t.name, "review");
        assert_eq!(t.description, "Review the staged changes carefully.");
    }
}
