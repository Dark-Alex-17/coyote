use super::*;

use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use dirs::home_dir;
use std::sync::LazyLock;

pub static SHELL: LazyLock<Shell> = LazyLock::new(detect_shell);

pub struct Shell {
    pub name: String,
    pub cmd: String,
    pub arg: String,
}

impl Shell {
    pub fn new(name: &str, cmd: &str, arg: &str) -> Self {
        Self {
            name: name.to_string(),
            cmd: cmd.to_string(),
            arg: arg.to_string(),
        }
    }
}

pub fn detect_shell() -> Shell {
    let cmd = env::var(get_env_name("shell")).ok().or_else(|| {
        if cfg!(windows) {
            if let Ok(ps_module_path) = env::var("PSModulePath") {
                let ps_module_path = ps_module_path.to_lowercase();
                if ps_module_path.starts_with(r"c:\users") {
                    return if ps_module_path.contains(r"\powershell\7\") {
                        Some("pwsh.exe".to_string())
                    } else {
                        Some("powershell.exe".to_string())
                    };
                }
            }
            None
        } else {
            env::var("SHELL").ok()
        }
    });
    let name = cmd
        .as_ref()
        .and_then(|v| Path::new(v).file_stem().and_then(|v| v.to_str()))
        .map(|v| {
            if v == "nu" {
                "nushell".into()
            } else {
                v.to_lowercase()
            }
        });
    let (cmd, name) = match (cmd.as_deref(), name.as_deref()) {
        (Some(cmd), Some(name)) => (cmd, name),
        _ => {
            if cfg!(windows) {
                ("cmd.exe", "cmd")
            } else {
                ("/bin/sh", "sh")
            }
        }
    };
    let shell_arg = match name {
        "powershell" => "-Command",
        "cmd" => "/C",
        _ => "-c",
    };
    Shell::new(name, cmd, shell_arg)
}

/// Runs `cmd` on the inherited descriptors and returns its exit code. The
/// shell passthrough and `-e` come through here, so under the TUI, where
/// the child would inherit a raw tty and the capture pipe, it refuses.
pub fn run_command<T: AsRef<OsStr>>(
    cmd: &str,
    args: &[T],
    envs: Option<HashMap<String, String>>,
) -> Result<i32> {
    if crate::tui::active() {
        bail!("Shell passthrough is not available in the TUI yet"); // TASK-315
    }
    wait_for_status(cmd, args, envs, Stdio::inherit())
}

fn wait_for_status<T: AsRef<OsStr>>(
    cmd: &str,
    args: &[T],
    envs: Option<HashMap<String, String>>,
    stdin: Stdio,
) -> Result<i32> {
    let status = Command::new(cmd)
        .args(args.iter())
        .envs(envs.unwrap_or_default())
        .stdin(stdin)
        .status()?;
    Ok(exit_code(status))
}

/// Like `wait_for_status`, but the wait is a poll the runtime can
/// interrupt: a caller that drops this future (an abort racing it in a
/// `select!`) kills and reaps the child instead of blocking on it.
async fn wait_for_status_interruptible<T: AsRef<OsStr>>(
    cmd: &str,
    args: &[T],
    stdin: Stdio,
) -> Result<i32> {
    let mut child = KillOnDrop(Command::new(cmd).args(args.iter()).stdin(stdin).spawn()?);
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    Ok(exit_code(status))
}

/// Like `run_command_with_output`, but the wait is one the runtime can
/// interrupt: a caller that drops this future kills the child instead of
/// blocking on it. Returns the exit code (see `exit_code`) with what the
/// child wrote to stdout and stderr.
async fn run_command_with_output_interruptible<T: AsRef<OsStr>>(
    cmd: &str,
    args: &[T],
    stdin: Stdio,
) -> Result<(i32, String, String)> {
    let output = tokio::process::Command::new(cmd)
        .args(args.iter())
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await?;
    let stdout = String::from_utf8(output.stdout).context("Invalid UTF-8 in stdout")?;
    let stderr = String::from_utf8(output.stderr).context("Invalid UTF-8 in stderr")?;
    Ok((exit_code(output.status), stdout, stderr))
}

/// A child that does not outlive the future waiting on it.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // Both fail only when the child is already reaped.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The exit code to report for `status`; a child ended by a signal gets
/// the shell's `128 + signal` so it never reads as success.
fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}

pub fn run_command_with_output<T: AsRef<OsStr>>(
    cmd: &str,
    args: &[T],
    envs: Option<HashMap<String, String>>,
) -> Result<(bool, String, String)> {
    let output = Command::new(cmd)
        .args(args.iter())
        .envs(envs.unwrap_or_default())
        .output()?;
    let status = output.status;
    let stdout = std::str::from_utf8(&output.stdout).context("Invalid UTF-8 in stdout")?;
    let stderr = std::str::from_utf8(&output.stderr).context("Invalid UTF-8 in stderr")?;

    if !status.success() {
        debug!("Command `{cmd}` exited with non-zero: {status}");
    }

    if !stdout.is_empty() {
        debug!("Command `{cmd}` exited with non-zero. stderr: {stderr}");
    }

    if !stderr.is_empty() {
        debug!("Command `{cmd}` executed successfully. stdout: {stdout}");
    }

    Ok((status.success(), stdout.to_string(), stderr.to_string()))
}

pub async fn run_loader_command(
    path: &str,
    extension: &str,
    loader_command: &str,
) -> Result<String> {
    let cmd_args = shell_words::split(loader_command)
        .with_context(|| anyhow!("Invalid document loader '{extension}': `{loader_command}`"))?;
    let mut use_stdout = true;
    let outpath = temp_file("-output-", "").display().to_string();
    let cmd_args: Vec<_> = cmd_args
        .into_iter()
        .map(|mut v| {
            if v.contains("$1") {
                v = v.replace("$1", path);
            }
            if v.contains("$2") {
                use_stdout = false;
                v = v.replace("$2", &outpath);
            }
            v
        })
        .collect();
    let cmd_eval = shell_words::join(&cmd_args);
    debug!("run `{cmd_eval}`");
    let (cmd, args) = cmd_args.split_at(1);
    let cmd = &cmd[0];
    // A loader needs nothing from the keyboard, so under the TUI it runs
    // with stdin closed rather than inheriting the raw tty the event
    // stream is reading.
    let stdin = if crate::tui::active() {
        Stdio::null()
    } else {
        Stdio::inherit()
    };
    if use_stdout {
        let (status, stdout, stderr) = run_command_with_output_interruptible(cmd, args, stdin)
            .await
            .with_context(|| {
                format!("Unable to run `{cmd_eval}`, Perhaps '{cmd}' is not installed?")
            })?;
        if status != 0 {
            let err = if !stderr.is_empty() {
                stderr
            } else {
                format!("The command `{cmd_eval}` exited with non-zero.")
            };
            bail!("{err}")
        }
        Ok(stdout)
    } else {
        let status = wait_for_status_interruptible(cmd, args, stdin)
            .await
            .with_context(|| {
                format!("Unable to run `{cmd_eval}`, Perhaps '{cmd}' is not installed?")
            })?;
        if status != 0 {
            bail!("The command `{cmd_eval}` exited with non-zero.")
        }
        let contents = std::fs::read_to_string(&outpath)
            .context("Failed to read file generated by the loader")?;
        Ok(contents)
    }
}

/// Whether an editor can take the terminal right now. Callers that write
/// a file for the editor to open check this first, so a refusal leaves
/// nothing behind.
pub fn ensure_editor_available() -> Result<()> {
    if crate::tui::active() {
        bail!("Editing files is not available in the TUI yet"); // TASK-315
    }
    Ok(())
}

/// Whether an inquire prompt can take the terminal right now. Every REPL
/// command that still asks on the raw tty checks this before it builds
/// the prompt, so under the full-screen UI the refusal lands in the
/// transcript instead of a question nobody can see.
pub fn ensure_prompts_available() -> Result<()> {
    if crate::tui::active() {
        bail!("Interactive prompts are not available in the TUI yet"); // TASK-315
    }
    Ok(())
}

pub fn edit_file(editor: &str, path: &Path) -> Result<()> {
    ensure_editor_available()?;
    let mut child = Command::new(editor).arg(path).spawn()?;
    child.wait()?;
    Ok(())
}

pub fn append_to_shell_history(shell: &str, command: &str, exit_code: i32) -> io::Result<()> {
    if let Some(history_file) = get_history_file(shell) {
        let command = command.replace('\n', " ");
        let now = now_timestamp();
        let history_txt = if shell == "fish" {
            format!("- cmd: {command}\n  when: {now}")
        } else if shell == "zsh" {
            format!(": {now}:{exit_code};{command}",)
        } else {
            command
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&history_file)?;
        writeln!(file, "{history_txt}")?;
    }
    Ok(())
}

fn get_history_file(shell: &str) -> Option<PathBuf> {
    match shell {
        "bash" | "sh" => env::var("HISTFILE")
            .ok()
            .map(PathBuf::from)
            .or(Some(home_dir()?.join(".bash_history"))),
        "zsh" => env::var("HISTFILE")
            .ok()
            .map(PathBuf::from)
            .or(Some(home_dir()?.join(".zsh_history"))),
        "nushell" => Some(dirs::config_dir()?.join("nushell").join("history.txt")),
        "fish" => Some(
            home_dir()?
                .join(".local")
                .join("share")
                .join("fish")
                .join("fish_history"),
        ),
        "powershell" | "pwsh" => {
            #[cfg(not(windows))]
            {
                Some(
                    home_dir()?
                        .join(".local")
                        .join("share")
                        .join("powershell")
                        .join("PSReadLine")
                        .join("ConsoleHost_history.txt"),
                )
            }
            #[cfg(windows)]
            {
                Some(
                    dirs::data_dir()?
                        .join("Microsoft")
                        .join("Windows")
                        .join("PowerShell")
                        .join("PSReadLine")
                        .join("ConsoleHost_history.txt"),
                )
            }
        }
        "ksh" => Some(home_dir()?.join(".ksh_history")),
        "tcsh" => Some(home_dir()?.join(".history")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// A loader killed by a signal reports a non-zero status, so its
    /// partial output is never read as success.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_signal_killed_loader_is_not_a_success() {
        let status = wait_for_status_interruptible("sh", &["-c", "kill -9 $$"], Stdio::null())
            .await
            .unwrap();
        assert_eq!(status, 128 + libc::SIGKILL);
        let status = wait_for_status("sh", &["-c", "kill -9 $$"], None, Stdio::null()).unwrap();
        assert_eq!(status, 128 + libc::SIGKILL);
    }

    /// Runs `loader` (a shell line that records its pid in `pid_file`
    /// and then sleeps for half a minute) under the TUI, aborts once the
    /// pid is on disk, and checks the caller gets the abort promptly and
    /// the loader process is gone, not left behind to finish on its own.
    #[cfg(unix)]
    async fn assert_aborted_loader_is_reaped(loader: &str, pid_file: &Path) {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);
        let abort_signal = create_abort_signal();
        let aborter = abort_signal.clone();
        let pid_path = pid_file.to_path_buf();
        let aborted_at = tokio::spawn(async move {
            while !matches!(std::fs::metadata(&pid_path), Ok(meta) if meta.len() > 0) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            aborter.set_ctrlc();
            std::time::Instant::now()
        });
        let (_, spinner_rx) = Spinner::create("Loading");
        let err = abortable_run_with_spinner_rx(
            run_loader_command("unused", "txt", loader),
            spinner_rx,
            abort_signal,
            UserInterrupt::NotOwned,
        )
        .await
        .unwrap_err();
        let took = aborted_at.await.unwrap().elapsed();
        assert!(err.to_string().contains("Aborted"), "{err}");
        assert!(took < Duration::from_secs(2), "the abort took {took:?}");

        let pid: i32 = std::fs::read_to_string(pid_file)
            .expect("the loader never wrote its pid")
            .trim()
            .parse()
            .unwrap();
        std::fs::remove_file(pid_file).unwrap();
        let mut gone = false;
        for _ in 0..20 {
            // A reaped pid answers ESRCH to a null signal; a zombie or a
            // still-running child answers 0. The wait yields to the
            // runtime because a `kill_on_drop` child is reaped by it.
            if unsafe { libc::kill(pid, 0) } == -1 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            gone,
            "the loader (pid {pid}) is still alive after the abort"
        );
    }

    /// A file-writing loader (`$2`) is polled, so an abort kills it.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn an_aborted_loader_returns_promptly_and_its_child_is_reaped() {
        let pid_file = temp_file("-loader-pid-", "");
        let loader = format!(
            "sh -c \"echo $$ > '{}'; exec sleep 30 # $2\"",
            pid_file.display()
        );
        assert_aborted_loader_is_reaped(&loader, &pid_file).await;
    }

    /// A stdout loader (no `$2`) waits on the runtime too, so an abort
    /// kills it instead of blocking on its output.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn an_aborted_stdout_loader_returns_promptly_and_its_child_is_reaped() {
        let pid_file = temp_file("-loader-pid-", "");
        let loader = format!(
            "sh -c \"echo $$ > '{}'; exec sleep 30\"",
            pid_file.display()
        );
        assert_aborted_loader_is_reaped(&loader, &pid_file).await;
    }

    #[cfg(unix)]
    #[test]
    fn a_status_with_a_code_keeps_it() {
        let status = Command::new("sh").args(["-c", "exit 3"]).status().unwrap();
        assert_eq!(exit_code(status), 3);
    }
}
