//! Integration tests for sandbox security and failure scenarios.
//!
//! These tests verify that the sandbox correctly detects and reports:
//! - Memory Limit Exceeded (MLE)
//! - Output Limit Exceeded (OLE)
//! - Time Limit Exceeded (TLE)
//! - Fork bomb prevention (PID limits)
//! - Filesystem escape prevention
//! - Compilation errors
//! - Concurrent job isolation

use jet_core::models::{ExecutionLimits, StageStatus};
use jet_server::sandbox::{Sandbox, SandboxProfile};
use tempfile::tempdir;

fn strict_profile() -> SandboxProfile {
    SandboxProfile::strict()
}

fn write_script(dir: &std::path::Path, name: &str, content: &str) {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("should write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("should set executable permissions");
    }
}

// ============================================================
// Time Limit Exceeded
// ============================================================

#[test]
fn detects_time_limit_exceeded() {
    let limits = ExecutionLimits {
        timeout_ms: 1000,
        ..ExecutionLimits::default()
    };
    let dir = tempdir().expect("tempdir");
    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let result = sandbox
        .run("/bin/sleep", &["10"], None, None, 1_000)
        .expect("run");

    assert_eq!(result.status, StageStatus::TimeLimitExceeded);
}

// ============================================================
// Memory Limit Exceeded
// ============================================================

#[test]
fn detects_memory_limit_exceeded() {
    let limits = ExecutionLimits {
        memory_limit_bytes: 16 * 1024 * 1024, // 16 MB
        timeout_ms: 5000,
        ..ExecutionLimits::default()
    };
    let dir = tempdir().expect("tempdir");

    // Write a Python script that tries to allocate a large amount of memory
    write_script(
        dir.path(),
        "oom.py",
        r#"
data = []
while True:
    data.append(b'X' * (1024 * 1024))  # 1 MB chunks
"#,
    );

    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let result = sandbox
        .run("/usr/bin/python3", &["oom.py"], None, None, 5_000)
        .expect("run");

    // Should be either MLE or RuntimeError (python may crash with MemoryError)
    assert!(
        matches!(
            result.status,
            StageStatus::MemoryLimitExceeded | StageStatus::RuntimeError
        ),
        "expected MLE or RuntimeError, got {:?}",
        result.status,
    );
    assert_ne!(result.status, StageStatus::Success);
}

// ============================================================
// Output Limit Exceeded
// ============================================================

#[test]
fn detects_output_limit_exceeded() {
    let limits = ExecutionLimits {
        output_limit_bytes: 1024, // 1 KB
        timeout_ms: 5000,
        ..ExecutionLimits::default()
    };
    let dir = tempdir().expect("tempdir");

    write_script(
        dir.path(),
        "bigout.sh",
        "#!/bin/sh\ndd if=/dev/zero bs=4096 count=1024 2>/dev/null\n",
    );

    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let result = sandbox
        .run("/bin/sh", &["bigout.sh"], None, None, 5_000)
        .expect("run");

    assert!(
        matches!(
            result.status,
            StageStatus::OutputLimitExceeded | StageStatus::RuntimeError
        ),
        "expected OLE or RuntimeError, got {:?}",
        result.status,
    );
    assert_ne!(result.status, StageStatus::Success);
}

// ============================================================
// Fork Bomb Prevention
// ============================================================

#[test]
fn prevents_fork_bomb() {
    let limits = ExecutionLimits {
        pid_limit: 8,
        timeout_ms: 5000,
        ..ExecutionLimits::default()
    };
    let dir = tempdir().expect("tempdir");

    write_script(dir.path(), "fork.sh", "#!/bin/sh\n:(){ :|:& };:\n");

    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let result = sandbox
        .run("/bin/sh", &["fork.sh"], None, None, 5_000)
        .expect("run");

    // Fork bomb should fail — it shouldn't succeed
    assert_ne!(
        result.status,
        StageStatus::Success,
        "fork bomb should not succeed"
    );
}

// ============================================================
// Filesystem Escape Prevention
// ============================================================

#[test]
fn cannot_read_etc_passwd() {
    let limits = ExecutionLimits {
        timeout_ms: 5000,
        ..ExecutionLimits::default()
    };
    let dir = tempdir().expect("tempdir");

    write_script(dir.path(), "escape.sh", "#!/bin/sh\ncat /etc/passwd\n");

    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let result = sandbox
        .run("/bin/sh", &["escape.sh"], None, None, 5_000)
        .expect("run");

    // With Landlock enabled, reading /etc/passwd should fail
    assert_ne!(
        result.status,
        StageStatus::Success,
        "reading /etc/passwd should fail in sandbox"
    );
    // stdout should NOT contain /etc/passwd content (root:x:0:0:...)
    assert!(
        !result.stdout.contains("root:"),
        "sandbox leaked /etc/passwd contents"
    );
}

#[test]
fn cannot_write_outside_workspace() {
    let limits = ExecutionLimits {
        timeout_ms: 5000,
        ..ExecutionLimits::default()
    };
    let dir = tempdir().expect("tempdir");

    write_script(
        dir.path(),
        "escape_write.sh",
        "#!/bin/sh\necho 'pwned' > /etc/hacked 2>&1\necho $?\n",
    );

    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let _result = sandbox
        .run("/bin/sh", &["escape_write.sh"], None, None, 5_000)
        .expect("run");

    // Writing to /etc should fail — the file should not exist
    assert!(!std::path::Path::new("/etc/hacked").exists());
}

// ============================================================
// Compilation Errors
// ============================================================

#[test]
fn compilation_error_returns_runtime_error_with_stderr() {
    let limits = ExecutionLimits {
        timeout_ms: 5000,
        ..ExecutionLimits::default()
    };
    let dir = tempdir().expect("tempdir");

    write_script(dir.path(), "bad.py", "def invalid syntax here:\n");

    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let result = sandbox
        .run(
            "/usr/bin/python3",
            &[
                "-c",
                "import py_compile; py_compile.compile('bad.py', doraise=True)",
            ],
            None,
            None,
            5_000,
        )
        .expect("run");

    assert_eq!(result.status, StageStatus::RuntimeError);
    assert!(
        !result.stderr.is_empty(),
        "stderr should contain compilation error details"
    );
}

// ============================================================
// Basic Success Path
// ============================================================

#[test]
fn echo_succeeds_in_strict_profile() {
    let limits = ExecutionLimits::default();
    let dir = tempdir().expect("tempdir");
    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let result = sandbox
        .run("/bin/echo", &["hello", "security"], None, None, 1_000)
        .expect("run");

    assert_eq!(result.status, StageStatus::Success);
    assert_eq!(result.stdout.trim(), "hello security");
}

// ============================================================
// Stdin Handling
// ============================================================

#[test]
fn stdin_data_is_passed_to_process() {
    let limits = ExecutionLimits::default();
    let dir = tempdir().expect("tempdir");
    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let result = sandbox
        .run("/bin/cat", &[], None, Some("input data\n"), 2_000)
        .expect("run");

    assert_eq!(result.status, StageStatus::Success);
    assert_eq!(result.stdout.trim(), "input data");
}

// ============================================================
// Exit Code Tracking
// ============================================================

#[test]
fn nonzero_exit_code_is_runtime_error() {
    let limits = ExecutionLimits::default();
    let dir = tempdir().expect("tempdir");
    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let result = sandbox
        .run("/bin/sh", &["-c", "exit 42"], None, None, 2_000)
        .expect("run");

    assert_eq!(result.status, StageStatus::RuntimeError);
    assert_eq!(result.exit_code, Some(42));
}

// ============================================================
// Network Isolation
// ============================================================

#[test]
fn network_is_isolated() {
    let limits = ExecutionLimits {
        timeout_ms: 5000,
        ..ExecutionLimits::default()
    };
    let dir = tempdir().expect("tempdir");

    write_script(
        dir.path(),
        "network.sh",
        "#!/bin/sh\nping -c 1 -W 1 8.8.8.8 2>&1\necho \"exit=$?\"\n",
    );

    let mut sandbox = Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

    let result = sandbox
        .run("/bin/sh", &["network.sh"], None, None, 5_000)
        .expect("run");

    // Network namespace should block outbound traffic
    // Either the command fails or it times out at the OS level
    assert!(
        result.stdout.contains("exit=1")
            || result.stdout.contains("exit=2")
            || result.status != StageStatus::Success,
        "network should be isolated in sandbox"
    );
}

// ============================================================
// Concurrent Job Isolation
// ============================================================

#[test]
fn concurrent_jobs_do_not_interfere() {
    use std::thread;

    let handles: Vec<_> = (0..4)
        .map(|i| {
            thread::spawn(move || {
                let limits = ExecutionLimits::default();
                let dir = tempdir().expect("tempdir");

                write_script(
                    dir.path(),
                    "job.sh",
                    &format!("#!/bin/sh\necho 'job-{i}'\nsleep 0.1\necho 'done-{i}'\n"),
                );

                let mut sandbox =
                    Sandbox::new(&limits, dir.path(), None, &strict_profile()).expect("sandbox");

                let result = sandbox
                    .run("/bin/sh", &["job.sh"], None, None, 3_000)
                    .expect("run");

                assert_eq!(result.status, StageStatus::Success);
                assert!(
                    result.stdout.contains(&format!("job-{i}")),
                    "job {i} output should contain its own marker"
                );
                assert!(
                    result.stdout.contains(&format!("done-{i}")),
                    "job {i} should complete"
                );
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("thread should complete");
    }
}
