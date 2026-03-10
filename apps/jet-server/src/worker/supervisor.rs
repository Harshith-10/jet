use std::path::{Path, PathBuf};

use jet_core::models::{ExecutionLimits, JobRequest, JobResult};
use jet_pack::manifest::RuntimeManifest;
use tokio::process::Command;
use tracing::{error, warn};

use super::evaluator::Evaluator;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChildEvalPayload {
    pub request: JobRequest,
    pub workspace_dir: PathBuf,
    pub runtime_root_dir: PathBuf,
    pub zig_cache_dir: Option<PathBuf>,
    pub manifest: RuntimeManifest,
    pub limits: ExecutionLimits,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ChildEvalOutput {
    pub result: Option<JobResult>,
    pub error: Option<String>,
}

pub async fn run_supervised_job(
    payload: ChildEvalPayload,
    wall_clock_limit: std::time::Duration,
) -> std::io::Result<JobResult> {
    let scratch_root = std::env::temp_dir().join("jet-supervisor");
    std::fs::create_dir_all(&scratch_root)?;

    let work_id = uuid::Uuid::new_v4().to_string();
    let input_path = scratch_root.join(format!("{work_id}.in.json"));
    let output_path = scratch_root.join(format!("{work_id}.out.json"));

    std::fs::write(&input_path, serde_json::to_vec(&payload)?)?;

    let current_exe = std::env::current_exe()?;
    let mut command = Command::new(current_exe);
    command
        .arg("--jet-child-eval")
        .arg("--input")
        .arg(&input_path)
        .arg("--output")
        .arg(&output_path)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    configure_supervised_command(&mut command);

    let mut child = command.spawn()?;
    let child_pid = child.id();

    let status = wait_for_supervised_exit(&mut child, child_pid, wall_clock_limit).await?;

    let output_raw = match std::fs::read(&output_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            cleanup_supervisor_artifacts(&input_path, &output_path);
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, format!(
                "child exited with status {status} but output was missing: {e}"
            )));
        }
    };

    let output: ChildEvalOutput = serde_json::from_slice(&output_raw)
        .map_err(|e| std::io::Error::other(format!("failed to decode child output JSON: {e}")))?;

    cleanup_supervisor_artifacts(&input_path, &output_path);

    if let Some(err) = output.error {
        return Err(std::io::Error::other(err));
    }

    output
        .result
        .ok_or_else(|| std::io::Error::other("child process returned no result"))
}

fn configure_supervised_command(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.as_std_mut().pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

async fn wait_for_supervised_exit(
    child: &mut tokio::process::Child,
    child_pid: Option<u32>,
    wall_clock_limit: std::time::Duration,
) -> std::io::Result<std::process::ExitStatus> {
    let wait_result = tokio::time::timeout(wall_clock_limit, child.wait()).await;
    match wait_result {
        Ok(status) => status,
        Err(_) => {
            if let Some(pid) = child_pid {
                kill_process_tree(pid);
            }
            let _ = child.wait().await;
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "job exceeded hard wall-clock timeout of {}s",
                    wall_clock_limit.as_secs()
                ),
            ))
        }
    }
}

pub fn maybe_run_child_eval_mode(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some("--jet-child-eval") {
        return None;
    }

    match run_child_eval_mode(args) {
        Ok(()) => Some(0),
        Err(e) => {
            error!(error = %e, "child-eval failed");
            Some(1)
        }
    }
}

fn run_child_eval_mode(args: &[String]) -> std::io::Result<()> {
    let mut input_path: Option<PathBuf> = None;
    let mut output_path: Option<PathBuf> = None;

    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "--input" => {
                i += 1;
                input_path = args.get(i).map(PathBuf::from);
            }
            "--output" => {
                i += 1;
                output_path = args.get(i).map(PathBuf::from);
            }
            _ => {}
        }
        i += 1;
    }

    let input_path = input_path.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing --input path")
    })?;
    let output_path = output_path.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing --output path")
    })?;

    let payload_raw = std::fs::read(&input_path)?;
    let payload: ChildEvalPayload = serde_json::from_slice(&payload_raw)
        .map_err(|e| std::io::Error::other(format!("invalid child payload: {e}")))?;

    let evaluator = Evaluator::new(
        payload.workspace_dir,
        Some(payload.runtime_root_dir),
        payload.zig_cache_dir,
        payload.manifest,
        payload.limits,
    );

    let eval_result = evaluator
        .evaluate(&payload.request)
        .map_err(|e| std::io::Error::other(e.to_string()));

    let output = match eval_result {
        Ok(result) => ChildEvalOutput {
            result: Some(result),
            error: None,
        },
        Err(e) => ChildEvalOutput {
            result: None,
            error: Some(e.to_string()),
        },
    };

    write_child_output(&output_path, &output)
}

fn write_child_output(path: &Path, output: &ChildEvalOutput) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec(output)?)
}

fn cleanup_supervisor_artifacts(input_path: &Path, output_path: &Path) {
    let _ = std::fs::remove_file(input_path);
    let _ = std::fs::remove_file(output_path);
}

#[cfg(unix)]
fn kill_process_tree(pid: u32) {
    let pgid = -(pid as i32);
    // Send SIGKILL to the process group to ensure descendants are terminated.
    let rc = unsafe { libc::kill(pgid, libc::SIGKILL) };
    if rc != 0 {
        warn!(pid = pid, error = %std::io::Error::last_os_error(), "failed to SIGKILL process group");
    }
}

#[cfg(not(unix))]
fn kill_process_tree(_pid: u32) {
    // Linux is the primary target, but keep non-unix builds compiling.
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        path::Path,
        process::Stdio,
        time::{Duration, Instant},
    };

    use tempfile::tempdir;
    use tokio::process::Command;

    use super::{configure_supervised_command, wait_for_supervised_exit};

    #[tokio::test]
    async fn hard_timeout_kills_process_group_descendants() {
        let tmp = tempdir().expect("tempdir");
        let bg_pid_path = tmp.path().join("bg.pid");
        let script = format!(
            "sleep 30 & echo $! > {} && exec sleep 30",
            shell_escape(bg_pid_path.as_path())
        );

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_supervised_command(&mut cmd);

        let mut child = cmd.spawn().expect("spawn child");
        let leader_pid = child.id().expect("child pid");

        let start = Instant::now();
        let err = wait_for_supervised_exit(&mut child, Some(leader_pid), Duration::from_millis(500))
            .await
            .expect_err("supervised wait should time out");

        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(5));

        // Give SIGKILL propagation a short moment.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let bg_pid_raw = std::fs::read_to_string(&bg_pid_path).expect("bg pid file should exist");
        let bg_pid: i32 = bg_pid_raw.trim().parse().expect("valid pid");

        assert!(!pid_exists(leader_pid as i32));
        assert!(!pid_exists(bg_pid));
    }

    fn pid_exists(pid: i32) -> bool {
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return true;
        }
        let errno = std::io::Error::last_os_error().raw_os_error();
        !matches!(errno, Some(code) if code == libc::ESRCH)
    }

    fn shell_escape(path: &Path) -> String {
        let s = path.to_string_lossy();
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
