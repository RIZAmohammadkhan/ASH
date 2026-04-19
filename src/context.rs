use std::path::Path;

use crate::shell::ShellRunResult;

const TOOL_HINTS: &[&str] = &[
    "git", "rg", "fd", "find", "node", "npm", "pnpm", "bun", "python3", "pip", "cargo", "rustc",
    "docker", "kubectl", "ffmpeg", "curl", "wget", "jq",
];

#[derive(Debug, Clone)]
pub struct PromptContext {
    pub cwd: String,
    pub shell: String,
    pub os: String,
    pub path_hint: String,
    pub last_command_result: Option<LastCommandSnapshot>,
}

#[derive(Debug, Clone)]
pub struct LastCommandSnapshot {
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl PromptContext {
    pub fn capture(shell: &str, cwd: &Path, last_result: Option<&ShellRunResult>) -> Self {
        Self {
            cwd: cwd.display().to_string(),
            shell: shell_name(shell),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            path_hint: build_path_hint(),
            last_command_result: last_result.map(|result| LastCommandSnapshot {
                command: result.command.clone(),
                exit_code: result.exit_code,
                stdout: result.stdout.clone(),
                stderr: result.stderr.clone(),
            }),
        }
    }

    pub fn to_block(&self) -> String {
        let mut block = format!(
            "CWD: {}\nShell: {}\nOS: {}\nPATH hint: {}\n",
            self.cwd, self.shell, self.os, self.path_hint
        );

        if let Some(last) = &self.last_command_result {
            let stdout = if last.stdout.trim().is_empty() {
                "[empty]"
            } else {
                &last.stdout
            };
            let stderr = if last.stderr.trim().is_empty() {
                "[empty]"
            } else {
                &last.stderr
            };

            block.push_str("Last command result:\n");
            block.push_str(&format!(
                "Command: {}\nExit code: {}\nSTDOUT:\n{}\nSTDERR:\n{}\n",
                last.command, last.exit_code, stdout, stderr
            ));
        }

        block
    }
}

fn build_path_hint() -> String {
    TOOL_HINTS
        .iter()
        .map(|tool| {
            if which::which(tool).is_ok() {
                format!("{tool}+")
            } else {
                format!("{tool}-")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_name(shell: &str) -> String {
    Path::new(shell)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(shell)
        .to_string()
}
