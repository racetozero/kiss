//! Agent Skills (agentskills.io style): Markdown files with YAML frontmatter,
//! discovered from user/project locations, surfaced in the system prompt and
//! as `/skill:name` commands.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub disable_model_invocation: bool,
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
}
