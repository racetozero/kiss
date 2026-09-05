//! Agent Skills (agentskills.io style): Markdown files with YAML frontmatter,
//! discovered from user/project locations, surfaced in the system prompt and
//! through `$name` or `/name` input tokens. The older `/skill:name` form is
//! also accepted.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInvocation {
    pub skill_names: Vec<String>,
    pub request: String,
}

/// Parse leading skill commands and `$name` skill mentions anywhere in user input.
pub fn parse_invocation(input: &str, skills: &[Skill]) -> Option<SkillInvocation> {
    let mut rest = input.trim_start();
    let mut skill_names = Vec::new();

    loop {
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_end];
        let candidate = token
            .strip_prefix('$')
            .or_else(|| token.strip_prefix("/skill:"))
            .or_else(|| token.strip_prefix('/'));
        let Some(skill) = candidate
            .filter(|name| !name.is_empty())
            .and_then(|name| skills.iter().find(|skill| skill.name == name))
        else {
            break;
        };
        if !skill_names.contains(&skill.name) {
            skill_names.push(skill.name.clone());
        }
        rest = rest[token_end..].trim_start();
    }

    for name in rest
        .split_whitespace()
        .filter_map(|token| token.strip_prefix('$'))
    {
        if let Some(skill) = skills.iter().find(|skill| skill.name == name)
            && !skill_names.contains(&skill.name)
        {
            skill_names.push(skill.name.clone());
        }
    }

    (!skill_names.is_empty()).then(|| SkillInvocation {
        skill_names,
        request: rest.trim().to_string(),
    })
}

/// Read invoked skill files and build text that is sent only to the model.
pub fn expand_invocation(invocation: &SkillInvocation, skills: &[Skill]) -> Result<String> {
    let mut output = String::from(
        "The user explicitly invoked the following skills. Follow each skill for this turn.\n\n",
    );
    for name in &invocation.skill_names {
        let skill = skills
            .iter()
            .find(|skill| &skill.name == name)
            .with_context(|| format!("invoked skill `{name}` is not available"))?;
        let content = std::fs::read_to_string(&skill.file_path).with_context(|| {
            format!(
                "could not read invoked skill `{name}` at {}",
                skill.file_path.display()
            )
        })?;
        output.push_str(&format!(
            "<invoked_skill name=\"{}\" path=\"{}\">\n{}\n</invoked_skill>\n\n",
            escape_xml(&skill.name),
            escape_xml(&skill.file_path.display().to_string()),
            content
        ));
    }
    output.push_str("<user_request>\n");
    output.push_str(&invocation.request);
    output.push_str("\n</user_request>");
    Ok(output)
}

/// Split a Markdown document into (frontmatter, body).
pub fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text.strip_prefix("---") else {
        return (None, text);
    };
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
        .unwrap_or(rest);
    for end in ["\n---\n", "\n---\r\n", "\r\n---\r\n", "\r\n---\n"] {
        if let Some(pos) = rest.find(end) {
            return (Some(&rest[..pos]), &rest[pos + end.len()..]);
        }
    }
    if let Some(stripped) = rest
        .strip_suffix("\n---")
        .or_else(|| rest.strip_suffix("\r\n---"))
    {
        return (Some(stripped), "");
    }
    (None, text)
}

fn parse_skill(path: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(path).ok()?;
    let (frontmatter, _body) = split_frontmatter(&text);
    let mut name = None;
    let mut description = None;
    let mut disable_model_invocation = false;
    if let Some(fm) = frontmatter
        && let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(fm)
    {
        name = value["name"].as_str().map(String::from);
        description = value["description"].as_str().map(String::from);
        disable_model_invocation = value["disable-model-invocation"].as_bool().unwrap_or(false);
    }
    let fallback_name = || {
        if path.file_name().and_then(|f| f.to_str()) == Some("SKILL.md") {
            path.parent()?.file_name()?.to_str().map(String::from)
        } else {
            path.file_stem()?.to_str().map(String::from)
        }
    };
    let name = name.or_else(fallback_name)?;
    Some(Skill {
        description: description.unwrap_or_else(|| format!("Skill {name}")),
        name,
        file_path: path.to_path_buf(),
        disable_model_invocation,
    })
}

/// Recursively find `SKILL.md` files under `dir`; when `include_root_md` is
/// set, direct `.md` children of `dir` also count as single-file skills.
fn scan_dir(dir: &Path, include_root_md: bool, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let skill_md = path.join("SKILL.md");
            if skill_md.is_file()
                && let Some(skill) = parse_skill(&skill_md)
            {
                out.push(skill);
            }
            scan_dir(&path, false, out);
        } else if include_root_md
            && path.extension().and_then(|e| e.to_str()) == Some("md")
            && path.file_name().and_then(|f| f.to_str()) != Some("SKILL.md")
            && let Some(skill) = parse_skill(&path)
        {
            out.push(skill);
        }
    }
}

/// Discover skills from every location pi checks (kiss equivalents).
pub fn discover(cwd: &Path, project_trusted: bool, extra_paths: &[PathBuf]) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        scan_dir(&home.join(".kiss/agent/skills"), true, &mut out);
        scan_dir(&home.join(".agents/skills"), false, &mut out);
    }
    if project_trusted {
        scan_dir(&cwd.join(".kiss/skills"), true, &mut out);
        // .agents/skills in cwd and ancestors up to a git root.
        for dir in cwd.ancestors() {
            scan_dir(&dir.join(".agents/skills"), false, &mut out);
            if dir.join(".git").exists() {
                break;
            }
        }
    }
    for path in extra_paths {
        if path.is_dir() {
            scan_dir(path, true, &mut out);
        } else if let Some(skill) = parse_skill(path) {
            out.push(skill);
        }
    }
    // Later discoveries with the same name lose (first location wins).
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.name.clone()));
    out
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "The following skills provide specialized instructions for specific tasks.\nRead the full skill file when the task matches its description.\nWhen a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n<available_skills>\n",
    );
    for skill in visible {
        out.push_str("  <skill>\n");
        out.push_str(&format!("    <name>{}</name>\n", escape_xml(&skill.name)));
        out.push_str(&format!(
            "    <description>{}</description>\n",
            escape_xml(&skill.description)
        ));
        out.push_str(&format!(
            "    <location>{}</location>\n",
            escape_xml(&skill.file_path.display().to_string())
        ));
        out.push_str("  </skill>\n");
    }
    out.push_str("</available_skills>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_split() {
        let (fm, body) = split_frontmatter("---\nname: x\n---\nbody here");
        assert_eq!(fm, Some("name: x"));
        assert_eq!(body, "body here");
        let (fm, body) = split_frontmatter("no frontmatter");
        assert!(fm.is_none());
        assert_eq!(body, "no frontmatter");
    }

    #[test]
    fn discovers_skill_dirs_and_root_md() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("pdf-tools")).unwrap();
        std::fs::write(
            root.join("pdf-tools/SKILL.md"),
            "---\nname: pdf-tools\ndescription: Work with PDFs\n---\nInstructions",
        )
        .unwrap();
        std::fs::write(
            root.join("quick.md"),
            "---\ndescription: Quick one\n---\nGo",
        )
        .unwrap();
        let mut out = Vec::new();
        scan_dir(root, true, &mut out);
        let names: Vec<&str> = out.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"pdf-tools"));
        assert!(names.contains(&"quick"));
    }

    #[test]
    fn prompt_xml_escapes() {
        let skills = vec![Skill {
            name: "a<b".into(),
            description: "uses & things".into(),
            file_path: "/s/SKILL.md".into(),
            disable_model_invocation: false,
        }];
        let xml = format_skills_for_prompt(&skills);
        assert!(xml.contains("<name>a&lt;b</name>"));
        assert!(xml.contains("uses &amp; things"));
    }

    fn skill(name: &str, path: PathBuf) -> Skill {
        Skill {
            name: name.into(),
            description: format!("Use {name}"),
            file_path: path,
            disable_model_invocation: false,
        }
    }

    #[test]
    fn parses_chained_dollar_slash_and_compatible_skill_tokens() {
        let skills = vec![
            skill("review", "review/SKILL.md".into()),
            skill("tests", "tests/SKILL.md".into()),
            skill("docs", "docs/SKILL.md".into()),
        ];
        assert_eq!(
            parse_invocation("  $review /tests /skill:docs check this", &skills),
            Some(SkillInvocation {
                skill_names: vec!["review".into(), "tests".into(), "docs".into()],
                request: "check this".into(),
            })
        );
        assert_eq!(
            parse_invocation("please use $review and $tests", &skills),
            Some(SkillInvocation {
                skill_names: vec!["review".into(), "tests".into()],
                request: "please use $review and $tests".into(),
            })
        );
        assert_eq!(parse_invocation("$unknown request", &skills), None);
    }

    #[test]
    fn expansion_reads_each_skill_once_and_keeps_the_request_last() {
        let dir = tempfile::tempdir().unwrap();
        let review = dir.path().join("review.md");
        let tests = dir.path().join("tests.md");
        std::fs::write(&review, "Review instructions").unwrap();
        std::fs::write(&tests, "Test instructions").unwrap();
        let skills = vec![skill("review", review), skill("tests", tests)];
        let invocation = parse_invocation("$review /tests inspect it", &skills).unwrap();

        let expanded = expand_invocation(&invocation, &skills).unwrap();

        assert_eq!(expanded.matches("Review instructions").count(), 1);
        assert_eq!(expanded.matches("Test instructions").count(), 1);
        assert!(expanded.ends_with("<user_request>\ninspect it\n</user_request>"));
    }

    #[test]
    fn expansion_error_names_an_unreadable_skill() {
        let skills = vec![skill("missing", "/no/such/skill/SKILL.md".into())];
        let invocation = parse_invocation("$missing run", &skills).unwrap();
        let error = expand_invocation(&invocation, &skills).unwrap_err();
        assert!(format!("{error:#}").contains("invoked skill `missing`"));
    }
}
