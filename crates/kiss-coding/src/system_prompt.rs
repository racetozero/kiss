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

fn rex_context(session: Option<&str>, block: Option<&str>) -> String {
    let valid = |value: &&str| {
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
    };
    let Some(session) = session.filter(valid) else {
        return String::new();
    };
    let mut context =
        format!("\n\nYou are running inside Rex. Your own REX_SESSION is {session:?}.");
    if let Some(block) = block.filter(valid) {
        context.push_str(&format!(" Your own terminal REX_BLOCK is {block:?}."));
    }
    context.push_str(
        "\nFor Rex tasks, load the rex skill if available. Otherwise run `rex skill show` and follow its guide. Use your own session and terminal for requests such as 'here' or 'beside you', or when no target is specified. Use the visible session or focused pane only when the user requests it. An explicit target name takes priority. Do not rename sessions unless the user or the installed skill requests it.",
    );
    context
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

    let file_read_tool = options
        .selected_tools
        .iter()
        .find(|tool| matches!(tool.as_str(), "read" | "bash"));
    if let Some(tool) = file_read_tool
        && !options.skills.is_empty()
    {
        prompt.push('\n');
        prompt.push_str(&crate::skills::format_skills_for_prompt_with_tool(
            options.skills,
            tool,
        ));
        prompt.push('\n');
    }

    if options.selected_tools.iter().any(|tool| tool == "bash") {
        let session = std::env::var("REX_SESSION").ok();
        let block = std::env::var("REX_BLOCK").ok();
        prompt.push_str(&rex_context(session.as_deref(), block.as_deref()));
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

    #[test]
    fn rex_context_identifies_own_location_and_rejects_invalid_ids() {
        let long = "x".repeat(257);
        for session in [None, Some(""), Some("bad\nlocation"), Some(long.as_str())] {
            assert!(rex_context(session, Some("block-1")).is_empty());
        }

        for block in [None, Some(""), Some("bad\x1bblock"), Some(long.as_str())] {
            let context = rex_context(Some("session-1"), block);
            assert!(context.contains("REX_SESSION is \"session-1\""));
            assert!(!context.contains("REX_BLOCK"));
            assert!(context.contains("rex skill show"));
        }

        let context = rex_context(Some("session-1"), Some("block-1"));
        assert!(context.contains("REX_SESSION is \"session-1\""));
        assert!(context.contains("REX_BLOCK is \"block-1\""));
        assert!(context.contains("An explicit target name takes priority"));
        assert!(rex_context(Some("a\"b"), None).contains(r#""a\"b""#));
    }

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
    fn skills_are_available_with_bash_as_the_only_reader() {
        let tools = vec!["bash".to_string()];
        let skill = Skill {
            name: "example".into(),
            description: "Example skill".into(),
            file_path: "/skills/example/SKILL.md".into(),
            disable_model_invocation: false,
        };
        let mut options = base_options(&tools);
        options.skills = std::slice::from_ref(&skill);
        let prompt = build_system_prompt(&options);
        assert!(prompt.contains("<name>example</name>"));
        assert!(prompt.contains("Use bash to load a skill's file"));

        let no_reader = vec!["edit".to_string()];
        options.selected_tools = &no_reader;
        assert!(!build_system_prompt(&options).contains("<available_skills>"));
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
