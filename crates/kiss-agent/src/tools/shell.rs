//! Command arguments shared by the agent, SDK, and terminal shell runners.

use std::path::PathBuf;
use tokio::process::Command;

pub fn resolve(shell_path: Option<&str>) -> PathBuf {
    shell_path
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            #[cfg(windows)]
            {
                windows_shell()
            }
            #[cfg(not(windows))]
            {
                PathBuf::from("bash")
            }
        })
}

#[cfg(any(windows, test))]
fn windows_shell() -> PathBuf {
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
    let paths = std::env::var_os("PATH").unwrap_or_default();
    let on_path = |name: &str| {
        std::env::split_paths(&paths)
            .filter(|directory| directory.is_absolute())
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
    };
    if let Some(bash) = on_path("bash.exe") {
        return bash;
    }
    for (name, variable, relative) in [
        ("pwsh.exe", "ProgramFiles", "PowerShell/7/pwsh.exe"),
        (
            "powershell.exe",
            "SystemRoot",
            "System32/WindowsPowerShell/v1.0/powershell.exe",
        ),
    ] {
        if let Some(shell) = on_path(name) {
            return shell;
        }
        if let Some(root) = std::env::var_os(variable) {
            let shell = PathBuf::from(root).join(relative);
            if shell.is_file() {
                return shell;
            }
        }
    }
    if let Some(cmd) = on_path("cmd.exe") {
        return cmd;
    }
    if let Some(cmd) = std::env::var_os("COMSPEC").map(PathBuf::from)
        && cmd.is_file()
        && cmd
            .file_stem()
            .is_some_and(|name| name.eq_ignore_ascii_case("cmd"))
    {
        return cmd;
    }
    std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32/cmd.exe"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("cmd.exe"))
}

pub fn command(shell_path: Option<&str>, source: &str, prefix: Option<&str>) -> Command {
    let shell = resolve(shell_path);
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

    #[test]
    fn windows_shell_selection() {
        if std::env::var_os("KISS_TEST_SHELL_PROBE").is_some() {
            #[cfg(windows)]
            let shell = command(None, "echo probe", None)
                .as_std()
                .get_program()
                .to_owned();
            #[cfg(not(windows))]
            let shell = windows_shell().into_os_string();
            println!("KISS_SELECTED_SHELL={}", shell.to_string_lossy());
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let program_files = root.join("Program Files");
        let windows = root.join("Windows");
        let path = root.join("path");
        let candidates = [
            program_files.join("Git/bin/bash.exe"),
            root.join("Program Files x86/Git/bin/bash.exe"),
            root.join("LocalAppData/Programs/Git/bin/bash.exe"),
            path.join("bash.exe"),
            path.join("pwsh.exe"),
            program_files.join("PowerShell/7/pwsh.exe"),
            path.join("powershell.exe"),
            windows.join("System32/WindowsPowerShell/v1.0/powershell.exe"),
            path.join("cmd.exe"),
            root.join("comspec/cmd.exe"),
            windows.join("System32/cmd.exe"),
        ];
        let probe = |expected: &std::path::Path| {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tools::shell::tests::windows_shell_selection",
                    "--nocapture",
                ])
                .env("KISS_TEST_SHELL_PROBE", "1")
                .env("ProgramFiles", &program_files)
                .env("ProgramFiles(x86)", root.join("Program Files x86"))
                .env("LOCALAPPDATA", root.join("LocalAppData"))
                .env("SystemRoot", &windows)
                .env("COMSPEC", root.join("comspec/cmd.exe"))
                .env("PATH", &path)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            let selected = stdout
                .lines()
                .find_map(|line| line.strip_prefix("KISS_SELECTED_SHELL="))
                .unwrap();
            assert_eq!(std::path::Path::new(selected), expected, "{stdout}");
        };

        // No Bash: choose an installed native Windows shell before command submission.
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("pwsh.exe"), "fixture").unwrap();
        probe(&path.join("pwsh.exe"));
        std::fs::remove_file(path.join("pwsh.exe")).unwrap();

        for candidate in &candidates {
            std::fs::create_dir_all(candidate.parent().unwrap()).unwrap();
            std::fs::write(candidate, "fixture").unwrap();
        }
        for candidate in &candidates {
            probe(candidate);
            std::fs::remove_file(candidate).unwrap();
        }
        probe(std::path::Path::new("cmd.exe"));
        let explicit = root.join("custom shell.exe");
        assert_eq!(
            command(explicit.to_str(), "echo test", None)
                .as_std()
                .get_program(),
            explicit.as_os_str()
        );
    }

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
