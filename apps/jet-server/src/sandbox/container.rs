use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use hakoniwa::{
    Container, Namespace, Rlimit, Runctl, Stdio,
    cgroups::{Memory, Pids, Resources},
    landlock::{CompatMode, FsAccess, Resource, Ruleset},
    seccomp::{Action, Arch, ArgCmp, Filter},
};
use jet_core::models::{ExecutionLimits, StageResult, StageStatus};
use tracing::warn;

use super::error::{SandboxError, SandboxResult};

#[derive(Debug, Clone)]
pub struct SandboxProfile {
    pub enable_cgroups: bool,
    pub enable_landlock: bool,
    pub enable_seccomp: bool,
    pub collect_metrics: bool,
}

impl SandboxProfile {
    pub fn strict() -> Self {
        Self {
            enable_cgroups: true,
            enable_landlock: false, // TEMP: disabled for debugging
            enable_seccomp: false, // Disabled for Java compatibility (requires complex syscall mapping)
            collect_metrics: true,
        }
    }
}

pub struct Sandbox {
    container: Container,
    /// Maximum bytes to read from stdout/stderr pipes before truncating.
    output_limit_bytes: u64,
    /// Physical memory limit — used to distinguish MLE from TLE on SIGKILL.
    memory_limit_bytes: u64,
}

impl Sandbox {
    pub fn new(
        limits: &ExecutionLimits,
        workspace_dir: &Path,
        runtime_dir: Option<&Path>,
        profile: &SandboxProfile,
    ) -> SandboxResult<Self> {
        Self::with_cache(limits, workspace_dir, runtime_dir, None, profile)
    }

    /// Like [`Self::new`] but also bind-mounts a compiler cache directory
    /// (read-write) at `/opt/zig-cache` inside the sandbox.
    pub fn with_cache(
        limits: &ExecutionLimits,
        workspace_dir: &Path,
        runtime_dir: Option<&Path>,
        cache_dir: Option<&Path>,
        profile: &SandboxProfile,
    ) -> SandboxResult<Self> {
        let mut container = Container::new();

        container
            // .unshare(Namespace::User)
            .unshare(Namespace::Mount)
            .unshare(Namespace::Pid)
            .unshare(Namespace::Cgroup)
            .unshare(Namespace::Ipc)
            .unshare(Namespace::Network)
            .unshare(Namespace::Uts);

        let workspace = workspace_dir.to_str().ok_or_else(|| {
            SandboxError::ExecutionFailed("workspace path is not valid UTF-8".to_string())
        })?;

        container
            .bindmount_ro("/bin", "/bin")
            .bindmount_ro("/lib", "/lib")
            .bindmount_ro("/usr", "/usr")
            .bindmount_rw(workspace, "/workspace")
            .devfsmount("/dev")
            .procfsmount("/proc")
            .tmpfsmount("/tmp");

        #[cfg(target_arch = "x86_64")]
        container.bindmount_ro("/lib64", "/lib64");

        if let Some(rt_dir) = runtime_dir {
            let runtime = rt_dir.to_str().ok_or_else(|| {
                SandboxError::ExecutionFailed("runtime path is not valid UTF-8".to_string())
            })?;
            container.bindmount_ro(runtime, "/opt/runtime");
        }

        if let Some(cd) = cache_dir {
            let cache = cd.to_str().ok_or_else(|| {
                SandboxError::ExecutionFailed("cache path is not valid UTF-8".to_string())
            })?;
            container.bindmount_rw(cache, "/opt/zig-cache");
        }

        // When cgroups are enabled, physical memory is enforced there.
        // RLIMIT_AS caps *virtual* address space – JVM-based runtimes
        // mmap large reserved (but uncommitted) regions for compressed
        // class space, code cache, metaspace, etc. so we allow 4x the
        // physical limit for virtual space.  Without cgroups, RLIMIT_AS
        // is the only memory backstop, so we keep it at the actual limit.
        let rlimit_as = if profile.enable_cgroups {
            limits.memory_limit_bytes.saturating_mul(4)
        } else {
            limits.memory_limit_bytes
        };

        container
            .setrlimit(Rlimit::As, rlimit_as, rlimit_as)
            .setrlimit(Rlimit::Core, 0, 0)
            .setrlimit(
                Rlimit::Fsize,
                limits.output_limit_bytes,
                limits.output_limit_bytes,
            )
            .setrlimit(Rlimit::Nofile, limits.file_limit, limits.file_limit)
            .setrlimit(Rlimit::Nproc, limits.pid_limit, limits.pid_limit);

        if profile.enable_cgroups {
            let mut resources = Resources::default();
            let mut memory = Memory::default();
            let mut pids = Pids::default();

            memory
                .limit(limits.memory_limit_bytes as i64)
                .reservation(limits.memory_limit_bytes as i64)
                .swap(limits.memory_limit_bytes as i64);
            pids.limit(limits.pid_limit as i64);
            resources.memory(memory).pids(pids);

            container.cgroups_resources(resources);
        }

        if profile.enable_landlock {
            let mut ruleset = Ruleset::default();
            ruleset.restrict(Resource::FS, CompatMode::Enforce);
            ruleset.add_fs_rule("/bin", FsAccess::from_str("r-x").unwrap());
            ruleset.add_fs_rule("/lib", FsAccess::from_str("r-x").unwrap());
            #[cfg(target_arch = "x86_64")]
            ruleset.add_fs_rule("/lib64", FsAccess::from_str("r-x").unwrap());
            ruleset.add_fs_rule("/usr", FsAccess::from_str("r-x").unwrap());
            ruleset.add_fs_rule("/dev", FsAccess::from_str("r--").unwrap());
            ruleset.add_fs_rule("/proc", FsAccess::from_str("r--").unwrap());
            ruleset.add_fs_rule("/tmp", FsAccess::from_str("rwx").unwrap());
            ruleset.add_fs_rule("/workspace", FsAccess::from_str("rwx").unwrap());
            if runtime_dir.is_some() {
                ruleset.add_fs_rule("/opt/runtime", FsAccess::from_str("r-x").unwrap());
            }
            container.landlock_ruleset(ruleset);
        }

        if profile.enable_seccomp {
            let mut filter = Filter::new(Action::Errno(libc::EPERM));
            #[cfg(target_arch = "x86_64")]
            {
                filter.add_arch(Arch::X8664);
                filter.add_arch(Arch::X86);
                filter.add_arch(Arch::X32);
            }
            #[cfg(target_arch = "aarch64")]
            {
                filter.add_arch(Arch::Aarch64);
            }

            let allowed_syscalls = [
                "access",
                "arch_prctl",
                "brk",
                "close",
                "execve",
                "exit_group",
                "fstat",
                "getrandom",
                "mmap",
                "mprotect",
                "munmap",
                "newfstatat",
                "openat",
                "pread64",
                "prlimit64",
                "read",
                "rseq",
                "set_robust_list",
                "set_tid_address",
                "stat",
                "write",
                "rt_sigaction",
                "rt_sigprocmask",
                "rt_sigreturn",
                "ioctl",
                "lseek",
                "futex",
                "clone",
                "wait4",
                "uname",
                "getcwd",
                "readlink",
                "sysinfo",
                "getuid",
                "getgid",
                "geteuid",
                "getegid",
                "getpid",
                "getppid",
                "gettid",
                "tgkill",
                "sigaltstack",
                "madvise",
                "clock_gettime",
                "fcntl",
                "dup",
                "dup2",
                "dup3",
                "pipe",
                "pipe2",
                "epoll_create1",
                "epoll_ctl",
                "epoll_wait",
                "eventfd2",
                "timerfd_create",
                "timerfd_settime",
                "timerfd_gettime",
                "sched_yield",
                "sched_getaffinity",
                "sched_setaffinity",
                "nanosleep",
                "clock_nanosleep",
                "mremap",
                "prctl",
                "getdents",
                "getdents64",
                "fadvise64",
                "mincore",
                "statfs",
                "fstatfs",
                "lstat",
                "newlstatat",
                "poll",
                "ppoll",
                "select",
                "pselect6",
                "vfork",
                "memfd_create",
            ];

            for syscall in allowed_syscalls {
                filter.add_rule(Action::Allow, syscall);
            }

            use hakoniwa::scmp_argcmp;
            filter.add_rule_conditional(Action::Allow, "personality", &[scmp_argcmp!(arg0 == 0)]);
            filter.add_rule_conditional(Action::Allow, "personality", &[scmp_argcmp!(arg0 == 8)]);
            container.seccomp_filter(filter);
        }

        if profile.collect_metrics {
            container.runctl(Runctl::GetProcPidStatus);
            container.runctl(Runctl::GetProcPidSmapsRollup);
        }

        Ok(Self {
            container,
            output_limit_bytes: limits.output_limit_bytes,
            memory_limit_bytes: limits.memory_limit_bytes,
        })
    }

    pub fn run(
        &mut self,
        cmd: &str,
        args: &[&str],
        envs: Option<&[(&str, &str)]>,
        stdin_data: Option<&str>,
        timeout_ms: u64,
    ) -> SandboxResult<StageResult> {
        let mut command = self.container.command(cmd);
        command.args(args);
        command.current_dir("/workspace");

        if let Some(envs) = envs {
            for (key, val) in envs {
                command.env(key, val);
            }
        }

        if stdin_data.is_some() {
            command.stdin(Stdio::piped());
        }
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let timeout_secs = std::cmp::max(1, timeout_ms.div_ceil(1000));
        command.wait_timeout(timeout_secs);

        let mut child = command.spawn()?;
        if let Some(data) = stdin_data {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(data.as_bytes());
            }
        }
        // Explicitly drop stdin so the child sees EOF.
        drop(child.stdin.take());

        // ── Read stdout/stderr with a byte cap ──────────────────────
        //
        // hakoniwa's `wait_with_output()` does an unbounded
        // `read_to_end` on both pipes.  A fast writer (e.g. `printf`
        // in a tight loop) can fill hundreds of MB into the pipe
        // before the timeout fires and kills the child, causing the
        // worker to OOM or stall.
        //
        // We take the pipes ourselves and read at most
        // `output_limit_bytes` from each, then kill the child if it
        // was still going.
        let max_bytes = self.output_limit_bytes as usize;
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let (stdout_bytes, stdout_truncated, stderr_bytes, stderr_truncated) =
            match (stdout_pipe, stderr_pipe) {
                (None, None) => (vec![], false, vec![], false),
                (Some(out), None) => {
                    let (data, trunc) = read_pipe_limited(out, max_bytes);
                    (data, trunc, vec![], false)
                }
                (None, Some(err)) => {
                    let (data, trunc) = read_pipe_limited(err, max_bytes);
                    (vec![], false, data, trunc)
                }
                (Some(out), Some(err)) => std::thread::scope(|s| {
                    let h_out = s.spawn(|| read_pipe_limited(out, max_bytes));
                    let h_err = s.spawn(|| read_pipe_limited(err, max_bytes));
                    let (out_data, out_trunc) =
                        h_out.join().unwrap_or_else(|_| (vec![], false));
                    let (err_data, err_trunc) =
                        h_err.join().unwrap_or_else(|_| (vec![], false));
                    (out_data, out_trunc, err_data, err_trunc)
                }),
            };

        let output_truncated = stdout_truncated || stderr_truncated;

        // If we hit the output cap, kill the child immediately so it
        // stops writing and we can proceed to `wait()`.
        if output_truncated {
            let _ = child.kill();
        }

        // Wait for exit status (the timeout/alarm will still fire if
        // the child hasn't died yet).
        let status = child.wait()?;
        let is_success = status.success();

        let reason_lc = status.reason.to_lowercase();
        let internal_code = status.code;
        let process_exit_code = status.exit_code;

        // ── Collect resource metrics ────────────────────────────────
        let mut memory_usage = None;
        let mut cpu_time = None;
        let mut execution_time = None;

        if let Some(rusage) = status.rusage {
            execution_time = Some(rusage.real_time.as_millis() as u64);
            let user_ms = rusage.user_time.as_millis();
            let system_ms = rusage.system_time.as_millis();
            cpu_time = Some((user_ms + system_ms) as u64);
            memory_usage = Some((rusage.max_rss as u64) * 1024);
        }

        if let Some(proc_status) = status.proc_pid_status {
            memory_usage = Some(proc_status.vmhwm * 1024);
        }

        // ── Determine stage status ──────────────────────────────────
        //
        // Priority order:
        //   1. Success
        //   2. Output-limit exceeded (truncation-based or SIGXFSZ)
        //   3. Memory-limit exceeded (heuristic on SIGKILL + peak RSS)
        //   4. Time-limit exceeded    (SIGKILL fallback)
        //   5. Runtime error
        let is_sigkill = internal_code == 128 + libc::SIGKILL
            || process_exit_code == Some(128 + libc::SIGKILL);
        let is_sigxfsz = internal_code == 128 + libc::SIGXFSZ
            || process_exit_code == Some(128 + libc::SIGXFSZ);
        let is_sigabrt = internal_code == 128 + libc::SIGABRT
            || process_exit_code == Some(128 + libc::SIGABRT);

        // Heuristic: if peak memory ≥ 80 % of the cgroup limit the
        // process was almost certainly OOM-killed, not timed-out.
        let mem_near_limit = memory_usage
            .map(|m| m >= self.memory_limit_bytes * 80 / 100)
            .unwrap_or(false);
        // Check stderr for allocation-failure messages (e.g. Rust
        // panics with "memory allocation of N bytes failed").
        let stderr_lc = String::from_utf8_lossy(&stderr_bytes).to_lowercase();
        let alloc_failure_in_stderr = stderr_lc.contains("memory allocation")
            && stderr_lc.contains("failed")
            || stderr_lc.contains("out of memory")
            || stderr_lc.contains("cannot allocate memory");
        let stage_status = if is_success && !output_truncated {
            StageStatus::Success
        } else if output_truncated
            || reason_lc.contains("output limit exceeded")
            || is_sigxfsz
        {
            StageStatus::OutputLimitExceeded
        } else if reason_lc.contains("cannot allocate memory")
            || reason_lc.contains("out of memory")
            || is_sigabrt
            || (is_sigkill && mem_near_limit)
            || alloc_failure_in_stderr
        {
            StageStatus::MemoryLimitExceeded
        } else if reason_lc.contains("timed out") || is_sigkill {
            StageStatus::TimeLimitExceeded
        } else {
            StageStatus::RuntimeError
        };

        if output_truncated {
            warn!(
                stdout_bytes = stdout_bytes.len(),
                stderr_bytes = stderr_bytes.len(),
                limit = max_bytes,
                "output truncated — classified as OLE"
            );
        }

        let signal = if is_sigkill {
            Some("SIGKILL".to_string())
        } else if is_sigxfsz {
            Some("SIGXFSZ".to_string())
        } else if is_sigabrt {
            Some("SIGABRT".to_string())
        } else {
            None
        };

        let mut stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();
        if stderr_str.trim().is_empty() && !is_success {
            stderr_str = status.reason.clone();
        }

        Ok(StageResult {
            status: stage_status,
            stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
            stderr: stderr_str,
            exit_code: process_exit_code,
            signal,
            memory_usage,
            cpu_time,
            execution_time,
        })
    }
}

/// Read from a pipe up to `max_bytes`, then stop.  Returns the data
/// collected and whether the limit was hit (i.e. the stream was
/// truncated and there was likely more data).
fn read_pipe_limited(mut reader: impl Read, max_bytes: usize) -> (Vec<u8>, bool) {
    // Pre-allocate conservatively (cap at 1 MiB initial alloc).
    let mut buf = Vec::with_capacity(max_bytes.min(1024 * 1024));
    let mut tmp = [0u8; 8192];

    loop {
        match reader.read(&mut tmp) {
            Ok(0) => return (buf, false), // EOF — not truncated
            Ok(n) => {
                let remaining = max_bytes.saturating_sub(buf.len());
                let to_keep = n.min(remaining);
                if to_keep > 0 {
                    buf.extend_from_slice(&tmp[..to_keep]);
                }
                if buf.len() >= max_bytes {
                    // Hit the cap.  Drop the reader so the child
                    // gets SIGPIPE on its next write.
                    return (buf, true);
                }
            }
            Err(_) => return (buf, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use jet_core::models::ExecutionLimits;
    use tempfile::tempdir;

    use super::{Sandbox, SandboxProfile};

    #[test]
    fn sandbox_echo_succeeds_with_portable_profile() {
        let limits = ExecutionLimits::default();
        let dir = tempdir().expect("tempdir");
        let mut sandbox =
            Sandbox::new(&limits, dir.path(), None, &SandboxProfile::strict()).expect("sandbox");

        let result = sandbox
            .run("/bin/echo", &["hello", "world"], None, None, 1_000)
            .expect("run");

        assert_eq!(result.status, jet_core::models::StageStatus::Success);
        assert_eq!(result.stdout, "hello world\n");
    }

    #[test]
    fn sandbox_timeout_maps_to_tle() {
        let limits = ExecutionLimits::default();
        let dir = tempdir().expect("tempdir");
        let mut sandbox =
            Sandbox::new(&limits, dir.path(), None, &SandboxProfile::strict()).expect("sandbox");

        let result = sandbox
            .run("/bin/sleep", &["2"], None, None, 1_000)
            .expect("run");

        assert_eq!(
            result.status,
            jet_core::models::StageStatus::TimeLimitExceeded
        );
    }
}
