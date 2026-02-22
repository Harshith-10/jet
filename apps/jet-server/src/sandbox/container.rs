use std::path::Path;
use std::str::FromStr;

use hakoniwa::{
    cgroups::{Memory, Pids, Resources},
    landlock::{CompatMode, FsAccess, Resource, Ruleset},
    seccomp::{Action, Arch, ArgCmp, Filter},
    Container, Namespace, Rlimit, Runctl, Stdio,
};
use jet_core::models::{ExecutionLimits, StageResult, StageStatus};

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
            enable_landlock: true,
            enable_seccomp: true,
            collect_metrics: true,
        }
    }

    pub fn portable() -> Self {
        Self {
            enable_cgroups: false,
            enable_landlock: false,
            enable_seccomp: false,
            collect_metrics: true,
        }
    }
}

pub struct Sandbox {
    container: Container,
}

impl Sandbox {
    pub fn new(
        limits: &ExecutionLimits,
        workspace_dir: &Path,
        runtime_dir: Option<&Path>,
        profile: &SandboxProfile,
    ) -> SandboxResult<Self> {
        let mut container = Container::new();

        container
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
            .tmpfsmount("/tmp");

        #[cfg(target_arch = "x86_64")]
        container.bindmount_ro("/lib64", "/lib64");

        if let Some(rt_dir) = runtime_dir {
            let runtime = rt_dir.to_str().ok_or_else(|| {
                SandboxError::ExecutionFailed("runtime path is not valid UTF-8".to_string())
            })?;
            container.bindmount_ro(runtime, "/opt/runtime");
        }

        container
            .setrlimit(Rlimit::As, limits.memory_limit_bytes, limits.memory_limit_bytes)
            .setrlimit(Rlimit::Core, 0, 0)
            .setrlimit(Rlimit::Fsize, limits.output_limit_bytes, limits.output_limit_bytes)
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

        Ok(Self { container })
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

        let timeout_secs = std::cmp::max(1, timeout_ms / 1000);
        command.wait_timeout(timeout_secs);

        let mut child = command.spawn()?;
        if let Some(data) = stdin_data {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(data.as_bytes());
            }
        }

        let output = child.wait_with_output()?;
        let status = output.status;
        let is_success = status.success();

        let reason_lc = status.reason.to_lowercase();
        let internal_code = status.code;
        let process_exit_code = status.exit_code;

        let stage_status = if is_success {
            StageStatus::Success
        } else if reason_lc.contains("timed out")
            || internal_code == 128 + libc::SIGKILL
            || process_exit_code == Some(128 + libc::SIGKILL)
        {
            StageStatus::TimeLimitExceeded
        } else if internal_code == 128 + libc::SIGXFSZ
            || process_exit_code == Some(128 + libc::SIGXFSZ)
        {
            StageStatus::OutputLimitExceeded
        } else if reason_lc.contains("cannot allocate memory") || reason_lc.contains("out of memory") {
            StageStatus::MemoryLimitExceeded
        } else {
            StageStatus::RuntimeError
        };

        let signal = if internal_code == 128 + libc::SIGKILL
            || process_exit_code == Some(128 + libc::SIGKILL)
        {
            Some("SIGKILL".to_string())
        } else if internal_code == 128 + libc::SIGXFSZ
            || process_exit_code == Some(128 + libc::SIGXFSZ)
        {
            Some("SIGXFSZ".to_string())
        } else {
            None
        };

        let mut memory_usage = None;
        let mut cpu_time = None;
        let mut execution_time = None;

        if let Some(rusage) = status.rusage {
            execution_time = Some((rusage.real_time.as_secs_f64() * 1000.0) as u64);
            cpu_time =
                Some(((rusage.user_time.as_secs_f64() + rusage.system_time.as_secs_f64()) * 1000.0) as u64);
            memory_usage = Some((rusage.max_rss as u64) * 1024);
        }

        if let Some(proc_status) = status.proc_pid_status {
            memory_usage = Some(proc_status.vmhwm * 1024);
        }

        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if stderr.trim().is_empty() && !is_success {
            stderr = status.reason.clone();
        }

        Ok(StageResult {
            status: stage_status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr,
            exit_code: process_exit_code,
            signal,
            memory_usage,
            cpu_time,
            execution_time,
        })
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
            Sandbox::new(&limits, dir.path(), None, &SandboxProfile::portable()).expect("sandbox");

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
            Sandbox::new(&limits, dir.path(), None, &SandboxProfile::portable()).expect("sandbox");

        let result = sandbox
            .run("/bin/sleep", &["2"], None, None, 1_000)
            .expect("run");

        assert_eq!(result.status, jet_core::models::StageStatus::TimeLimitExceeded);
    }
}
