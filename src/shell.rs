use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct ShellRunResult {
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub display_output: String,
    pub saved_output_path: Option<PathBuf>,
    pub open_target: Option<String>,
    pub new_cwd: PathBuf,
}

pub async fn run_command(
    shell_program: &str,
    command_text: &str,
    cwd: &Path,
) -> Result<ShellRunResult> {
    let (program, args) = shell_invocation(shell_program, command_text);
    let output = Command::new(&program)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("failed to execute {command_text}"))?;

    let stdout_raw = String::from_utf8_lossy(&output.stdout);
    
    let mut parsed_stdout = String::new();
    let mut new_cwd = cwd.to_path_buf();
    let mut in_cwd = false;
    
    for line in stdout_raw.lines() {
        if line.trim() == "ASH_CWD_START" {
            in_cwd = true;
            continue;
        }
        if in_cwd {
            let path = PathBuf::from(line.trim());
            if path.exists() {
                new_cwd = path;
            }
        } else {
            parsed_stdout.push_str(line);
            parsed_stdout.push('\n');
        }
    }

    let stdout = normalize_output(&parsed_stdout);
    let stderr = normalize_output(&String::from_utf8_lossy(&output.stderr));
    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let combined = combine_streams(&stdout, &stderr);
    let (display_output, saved_output_path) = prepare_display_output(&combined).await?;
    let open_target = saved_output_path
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| first_url(&combined));

    Ok(ShellRunResult {
        command: command_text.to_string(),
        stdout,
        stderr,
        exit_code,
        display_output,
        saved_output_path,
        open_target,
        new_cwd,
    })
}

pub fn should_retry(command: &str, result: &ShellRunResult) -> bool {
    let stderr = result.stderr.to_ascii_lowercase();
    let stdout = result.stdout.to_ascii_lowercase();

    if result.exit_code != 0 {
        return true;
    }

    if stderr.contains("not found")
        || stderr.contains("permission denied")
        || stderr.contains("timed out")
        || stderr.contains("no such file")
    {
        return true;
    }

    if command.trim_start().starts_with("curl")
        && (stdout.contains("404") || stdout.contains("\"error\"") || stdout.contains("not found"))
    {
        return true;
    }

    result.stdout.trim().is_empty() && likely_expected_output(command)
}

pub fn build_attempt_summary(command: &str, result: &ShellRunResult) -> String {
    let stdout = if result.stdout.trim().is_empty() {
        "[empty]"
    } else {
        &result.stdout
    };
    let stderr = if result.stderr.trim().is_empty() {
        "[empty]"
    } else {
        &result.stderr
    };

    format!(
        "Command: {}\nExit code: {}\nSTDOUT:\n{}\nSTDERR:\n{}",
        command, result.exit_code, stdout, stderr
    )
}

fn shell_invocation(shell_program: &str, command_text: &str) -> (String, Vec<String>) {
    let shell_name = Path::new(shell_program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(shell_program)
        .to_ascii_lowercase();

    let args = match shell_name.as_str() {
        "cmd" | "cmd.exe" => vec!["/C".to_string(), format!("{} & echo ASH_CWD_START & cd", command_text)],
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => vec![
            "-NoLogo".to_string(),
            "-Command".to_string(),
            format!("{}\nWrite-Output \"ASH_CWD_START\"\n(Get-Location).Path", command_text),
        ],
        _ => vec!["-lc".to_string(), format!("{}\necho \"ASH_CWD_START\"\npwd", command_text)],
    };

    (shell_program.to_string(), args)
}

fn normalize_output(value: &str) -> String {
    value.replace("\r\n", "\n").trim_end().to_string()
}

fn combine_streams(stdout: &str, stderr: &str) -> String {
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, true) => "[no output]".to_string(),
        (false, true) => stdout.to_string(),
        (true, false) => format!("STDERR:\n{stderr}"),
        (false, false) => format!("STDOUT:\n{stdout}\n\nSTDERR:\n{stderr}"),
    }
}

async fn prepare_display_output(output: &str) -> Result<(String, Option<PathBuf>)> {
    Ok((output.to_string(), None))
}

fn likely_expected_output(command: &str) -> bool {
    let command = command.to_ascii_lowercase();
    [
        "grep ",
        "rg ",
        "find ",
        "ls",
        "cat ",
        "curl ",
        "git show",
        "git diff",
        "docker ps",
        "kubectl get",
    ]
    .iter()
    .any(|needle| command.contains(needle))
}

fn first_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|token| token.starts_with("https://") || token.starts_with("http://"))
        .map(|token| {
            token
                .trim_matches(|character| matches!(character, ')' | ']' | '}' | ',' | ';'))
                .to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::{ShellRunResult, first_url, should_retry};

    fn result(stdout: &str, stderr: &str, exit_code: i32) -> ShellRunResult {
        ShellRunResult {
            command: "echo".to_string(),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit_code,
            display_output: stdout.to_string(),
            saved_output_path: None,
            open_target: None,
            new_cwd: PathBuf::new(),
        }
    }

    #[test]
    fn detects_openable_urls() {
        let url = first_url("See https://openrouter.ai/docs for details");
        assert_eq!(url.as_deref(), Some("https://openrouter.ai/docs"));
    }

    #[test]
    fn retries_failed_commands() {
        assert!(should_retry("ls missing", &result("", "not found", 1)));
    }
}
