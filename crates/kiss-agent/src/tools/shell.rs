//! Command arguments shared by the agent, SDK, and terminal shell runners.

use std::path::PathBuf;
use tokio::process::Command;

pub fn command(shell_path: Option<&str>, source: &str, prefix: Option<&str>) -> Command {
    let shell = shell_path
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            #[cfg(windows)]
            for (variable, directory) in [
                ("ProgramFiles", "Git"),
                ("ProgramFiles(x86)", "Git"),
                ("LOCALAPPDATA", "Programs/Git"),
            ] {
                if let Some(root) = std::env::var_os(variable) {
                    let bash = PathBuf::from(root).join(directory).join("bin/bash.exe");
                    if bash.is_file() {
                        return bash;
                    }
                }
            }
            PathBuf::from("bash")
        });
    let name = shell
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let source = match prefix {
        Some(prefix) if name == "cmd" => format!("{prefix} & {source}"),
        Some(prefix) => format!("{prefix}\n{source}"),
        None => source.to_string(),
    };
    let mut command = Command::new(shell);
    #[cfg(windows)]
    if name == "cmd" {
        // cmd.exe parses command text itself; C runtime argument escaping is unsuitable.
        command.args(["/D", "/S", "/C"]);
        command.raw_arg(format!("\"{source}\""));
        return command;
    }
    if matches!(name.as_str(), "powershell" | "pwsh") {
        command.args(["-NoProfile", "-NonInteractive", "-Command"]);
    } else {
        command.arg("-c");
    }
    command.arg(source);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preserves_quoted_url_prefix_and_exit_status() {
        let url = "https://cursor.com/loginDeepControl?challenge=test&uuid=test&mode=login";
        let mut cases = vec![(
            None,
            format!("printf '%s\\n' '{url}' \"$KISS_TEST_PREFIX\"; exit 7"),
            "KISS_TEST_PREFIX='prefix value'",
            format!("{url}\nprefix value\n"),
        )];
        if cfg!(windows) {
            cases.extend([
                (
                    Some("cmd.exe"),
                    format!("echo \"{url}\"& exit /b 7"),
                    "echo prefix value",
                    format!("prefix value\n\"{url}\"\n"),
                ),
                (
                    Some("powershell.exe"),
                    format!("Write-Output '{url}'; Write-Output $env:KISS_TEST_PREFIX; exit 7"),
                    "$env:KISS_TEST_PREFIX='prefix value'",
                    format!("{url}\nprefix value\n"),
                ),
            ]);
        }
        for (shell, source, prefix, expected) in cases {
            let output = command(shell, &source, Some(prefix))
                .output()
                .await
                .unwrap();
            assert_eq!(output.status.code(), Some(7), "{shell:?}: {output:?}");
            assert_eq!(
                String::from_utf8(output.stdout)
                    .unwrap()
                    .lines()
                    .map(str::trim_end)
                    .collect::<Vec<_>>(),
                expected.lines().collect::<Vec<_>>(),
                "{shell:?}"
            );
        }
    }
}
