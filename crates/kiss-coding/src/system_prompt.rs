//! System prompt assembly: identity + tool list + guidelines, appended
//! custom text, project context blocks, skills XML, and the cwd line.

use crate::context_files::ContextFile;
use crate::skills::Skill;

pub struct SystemPromptOptions<'a> {
    /// Replaces the default prompt body (context/skills/cwd still append).
    pub custom_prompt: Option<&'a str>,
    pub append: Option<&'a str>,
    pub selected_tools: &'a [String],
    pub cwd: &'a str,
    pub context_files: &'a [ContextFile],
    pub skills: &'a [Skill],
}

fn tool_snippet(name: &str) -> Option<&'static str> {
    Some(match name {
        "read" => "Read file contents (text and images)",
        "bash" => "Execute shell commands",
        "edit" => "Make targeted text replacements in files",
        "write" => "Create or overwrite files",
        "grep" => "Search file contents for patterns (respects .gitignore)",
        "find" => "Find files by glob pattern (respects .gitignore)",
        "ls" => "List directory contents",
        _ => return None,
    })
}

pub fn build_system_prompt(options: &SystemPromptOptions) -> String {
    let cwd = options.cwd.replace('\\', "/");
    let mut prompt = match options.custom_prompt {
        Some(custom) => custom.to_string(),
        None => default_prompt(options),
    };

    if let Some(append) = options.append {
        prompt.push_str("\n\n");
        prompt.push_str(append);
    }

    if !options.context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for file in options.context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                file.path.display(),
                file.content
            ));
        }
        prompt.push_str("</project_context>\n");
    }

    let has_read = options.selected_tools.iter().any(|t| t == "read");
    if has_read && !options.skills.is_empty() {
        prompt.push('\n');
        prompt.push_str(&crate::skills::format_skills_for_prompt(options.skills));
        prompt.push('\n');
    }

    prompt.push_str(&format!("\nCurrent working directory: {cwd}"));
    prompt
}

fn default_prompt(options: &SystemPromptOptions) -> String {
    let visible: Vec<String> = options
        .selected_tools
        .iter()
        .filter_map(|name| tool_snippet(name).map(|snippet| format!("- {name}: {snippet}")))
        .collect();
    let tools_list = if visible.is_empty() {
        "(none)".to_string()
    } else {
        visible.join("\n")
    };

    let mut guidelines: Vec<&str> = Vec::new();
    let has = |t: &str| options.selected_tools.iter().any(|x| x == t);
    if has("bash") && !has("grep") && !has("find") && !has("ls") {
        guidelines.push("Use bash for file operations like ls, rg, find");
    }
    guidelines.push("Be concise in your responses");
    guidelines.push("Show file paths clearly when working with files");
    let guidelines_list = guidelines
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are an expert coding assistant operating inside kiss, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n{tools_list}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.\n\nGuidelines:\n{guidelines_list}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_options<'a>(tools: &'a [String]) -> SystemPromptOptions<'a> {
        SystemPromptOptions {
            custom_prompt: None,
            append: None,
            selected_tools: tools,
            cwd: "/work",
            context_files: &[],
            skills: &[],
        }
    }

    #[test]
    fn default_prompt_lists_tools_and_cwd() {
        let tools = vec!["read".to_string(), "bash".to_string()];
        let p = build_system_prompt(&base_options(&tools));
        assert!(p.contains("- read:"));
        assert!(p.contains("- bash:"));
        assert!(p.contains("Use bash for file operations"));
        assert!(p.ends_with("Current working directory: /work"));
    }

    #[test]
    fn custom_prompt_keeps_appends() {
        let tools = vec!["read".to_string()];
        let mut o = base_options(&tools);
        o.custom_prompt = Some("You are a poet.");
        o.append = Some("Rhyme everything.");
        let p = build_system_prompt(&o);
        assert!(p.starts_with("You are a poet."));
        assert!(p.contains("Rhyme everything."));
        assert!(!p.contains("expert coding assistant"));
    }

    #[test]
    fn context_files_wrapped() {
        let tools = vec!["read".to_string()];
        let files = vec![ContextFile {
            path: "/p/AGENTS.md".into(),
            content: "be nice".into(),
        }];
        let mut o = base_options(&tools);
        o.context_files = &files;
        let p = build_system_prompt(&o);
        assert!(p.contains("<project_context>"));
        assert!(p.contains("<project_instructions path=\"/p/AGENTS.md\">"));
        assert!(p.contains("be nice"));
    }
}
