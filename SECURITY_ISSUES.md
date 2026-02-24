# Jet Security Issues & Resolution Plan

Date: 2026-02-23
Status: Code Review Complete - Critical Issues Identified

---

## Executive Summary

The Jet project is well-architected but has **4 critical security issues** and **6 medium/low priority issues** that must be addressed before production deployment. The most urgent is the missing namespace unsharing in the sandbox implementation.

**Overall Security Assessment:** ⚠️ **Not Production Ready**

| Severity | Count | Status |
|----------|-------|--------|
| 🔴 Critical | 4 | Not Fixed |
| 🟡 Medium | 4 | Not Fixed |
| 🟢 Low | 6 | Not Fixed |

---

## CRITICAL SECURITY ISSUES 🔴

### 1. Missing Namespace Unsharing (User, Mount, PID)

**File:** [apps/jet-server/src/sandbox/container.rs](apps/jet-server/src/sandbox/container.rs#L50-L60)

**Severity:** 🔴 CRITICAL

**Description:**
The sandbox only unshares 4 namespaces but the PLAN requires 7. The missing three are essential for isolation:

```rust
// Current implementation - INCOMPLETE
container
    .unshare(Namespace::Cgroup)
    .unshare(Namespace::Ipc)
    .unshare(Namespace::Network)
    .unshare(Namespace::Uts);

// Missing:
// - User namespace: Without this, code runs as the same user as parent
// - Mount namespace: Critical for filesystem isolation
// - PID namespace: Prevents containers from seeing each other's processes
```

**Security Impact:** HIGH
- User code runs as the same user as the Jet worker process
- User code can potentially see and interfere with parent process mounts
- Multiple concurrent jobs can see each other's PIDs
- Cross-job interference is possible

**Required Fix:**
```rust
container
    .unshare(Namespace::User)      // ADD THIS
    .unshare(Namespace::Mount)     // ADD THIS
    .unshare(Namespace::Pid)       // ADD THIS
    .unshare(Namespace::Cgroup)
    .unshare(Namespace::Ipc)
    .unshare(Namespace::Network)
    .unshare(Namespace::Uts);
```

**Effort:** 5 minutes

**Testing:** Verify that:
- User code getuid() returns non-zero
- User code cannot access parent filesystem outside /workspace
- Multiple jobs cannot see each other's PIDs

---

### 2. Security Profile Never Enforced (Cgroups, Landlock, Seccomp Disabled)

**File:** [apps/jet-server/src/sandbox/container.rs](apps/jet-server/src/sandbox/container.rs#L22-L34) and [apps/jet-server/src/worker/evaluator.rs](apps/jet-server/src/worker/evaluator.rs#L113)

**Severity:** 🔴 CRITICAL

**Description:**
Two security profiles exist but only the insecure one is used:

```rust
// STRICT profile - Has all security features (NEVER USED)
pub fn strict() -> Self {
    Self {
        enable_cgroups: true,      // Resource limits via cgroups v2
        enable_landlock: true,     // Filesystem access control
        enable_seccomp: true,      // System call filtering
        collect_metrics: true,
    }
}

// PORTABLE profile - Disables everything (ALWAYS USED)
pub fn portable() -> Self {
    Self {
        enable_cgroups: false,     // ❌ Resource limits DISABLED
        enable_landlock: false,    // ❌ Filesystem isolation DISABLED
        enable_seccomp: false,     // ❌ Syscall filtering DISABLED
        collect_metrics: true,
    }
}
```

**Current Usage:**
```rust
// apps/jet-server/src/worker/evaluator.rs - Line 113
let mut sandbox = Sandbox::new(
    &compile_limits,
    &self.workspace_dir,
    self.runtime_dir.as_deref(),
    &SandboxProfile::portable(),  // ❌ ALWAYS PORTABLE
)?;
```

**Security Impact:** CRITICAL
- No cgroups: Resource limits (memory, CPU, pids) NOT enforced via OS
- No Landlock: Filesystem access restrictions NOT enforced
- No Seccomp: Syscalls NOT filtered - user code can call any syscall
- Combined: These are the last line of defense if namespaces are escaped

**Fix Options:**

**Option A: Always Use Strict Profile (Recommended)**
```rust
&SandboxProfile::strict()  // Change line 113, 156, 208
```
- ✅ Simplest
- ✅ Most secure
- ⚠️ May have compatibility issues with old kernels (< 5.10 for Landlock)

**Option B: Make Profile Configurable**
Add to `JetConfig`:
```rust
pub struct JetConfig {
    // ... existing fields ...
    pub sandbox_profile: String,  // "strict" or "portable"
}
```

Then in main.rs:
```rust
let profile = match config.sandbox_profile.as_str() {
    "strict" => SandboxProfile::strict(),
    "portable" => SandboxProfile::portable(),
    _ => SandboxProfile::strict(), // Default to secure
};
```

And use profile in worker:
```rust
let mut sandbox = Sandbox::new(
    &limits,
    &workspace_dir,
    runtime_dir.as_deref(),
    &profile,  // Use the configured profile
)?;
```

**Effort:** 
- Option A: 5 minutes (change 3 lines)
- Option B: 30 minutes (add config, pass through context, update evaluator)

**Recommendation:** Start with Option A for immediate security. Plan Option B for future configurability.

---

### 3. Path Traversal Risk in Archive Extraction

**File:** [crates/jet-pack/src/archive.rs](crates/jet-pack/src/archive.rs#L56-L90)

**Severity:** 🔴 CRITICAL

**Description:**
ZIP archive extraction uses `enclosed_name()` for basic protection but doesn't validate final paths:

```rust
fn extract_zip(archive_path: &Path, destination: &Path) -> JetPackResult<()> {
    let file = fs::File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;

    for index in 0..archive.len() {
        let mut zipped = archive.by_index(index)?;

        let Some(enclosed_path) = zipped.enclosed_name() else {
            continue;  // ✅ Rejects paths with ..
        };

        let output_path = destination.join(enclosed_path);  // ❌ No validation

        if zipped.is_dir() {
            fs::create_dir_all(&output_path)?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;  // ⚠️ Could escape if output_path is bad
        }

        let mut out = fs::File::create(&output_path)?;
        io::copy(&mut zipped, &mut out)?;
    }

    Ok(())
}
```

**Attack Scenarios:**
1. ZIP with symlink pointing outside destination
2. Archive created on case-insensitive filesystem, extracted on case-sensitive
3. Path normalization differences between platforms

**Security Impact:** HIGH
- Runtime packages could escape destination directory
- Malicious archives could overwrite system files if destination is predictable
- Could lead to arbitrary file write on system

**Fix:**
```rust
fn extract_zip(archive_path: &Path, destination: &Path) -> JetPackResult<()> {
    let canonical_dest = destination
        .canonicalize()
        .or_else(|_| {
            fs::create_dir_all(destination)?;
            destination.canonicalize()
        })
        .map_err(|source| JetPackError::Io {
            path: destination.to_path_buf(),
            source,
        })?;

    let file = fs::File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;

    for index in 0..archive.len() {
        let mut zipped = archive.by_index(index)?;

        let Some(enclosed_path) = zipped.enclosed_name() else {
            continue;
        };

        let output_path = canonical_dest.join(enclosed_path);

        // ✅ NEW: Validate that resolved path stays within destination
        let canonical_output = output_path
            .canonicalize()
            .or_else(|_| Ok(output_path.clone()))  // Allow non-existent paths
            .map_err(|source| JetPackError::Io {
                path: output_path.clone(),
                source,
            })?;

        if !canonical_output.starts_with(&canonical_dest) {
            return Err(JetPackError::Io {
                path: output_path.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("path traversal detected: {:?}", enclosed_path),
                ),
            });
        }

        if zipped.is_dir() {
            fs::create_dir_all(&output_path)?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut out = fs::File::create(&output_path)?;
        io::copy(&mut zipped, &mut out)?;
    }

    Ok(())
}
```

Also for tar.gz extraction, use similar validation.

**Effort:** 20 minutes

**Testing:**
```bash
# Create malicious ZIP with path traversal
mkdir -p /tmp/zip_test
echo "bad content" > /tmp/zip_test/bad.txt
cd /tmp/zip_test && zip -r -y runtime.zip ../../bad.txt
# Verify extraction fails or stays in bounds
```

---

### 4. No Input Validation / Rate Limiting on API Endpoints

**File:** [apps/jet-server/src/api.rs](apps/jet-server/src/api.rs#L48-L60)

**Severity:** 🔴 CRITICAL

**Description:**
The job submission endpoint has no request size limits or validation:

```rust
async fn submit_job(
    State(state): State<ApiState>,
    Json(mut request): Json<JobRequest>,  // ⚠️ No size limit configured
) -> Result<(StatusCode, Json<SubmitJobResponse>), (StatusCode, String)> {
    let requested = request
        .version
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "version is required".to_string()))?;
    // ...
}
```

**Attack Scenarios:**
1. POST 10GB of file content → OOM the server
2. Millions of duplicate files → Exhaust storage/memory
3. Infinite loops in submitted code → Consume all workers
4. No rate limiting → Weaponize for DDoS

**Security Impact:** HIGH
- DoS via large payload
- Memory exhaustion
- Resource starvation
- Queue poisoning

**Fix Options:**

**Option A: Configure Axum with DefaultBodyLimit (Recommended)**
```rust
// In apps/jet-server/src/main.rs, add:
use axum::extract::DefaultBodyLimit;

let app = api::router(api_state)
    .layer(DefaultBodyLimit::max(50 * 1024 * 1024))  // 50 MB limit
    .into_make_service_with_connect_info::<SocketAddr>();
```

**Option B: Add Custom Middleware**
```rust
// Create middleware to validate request size
use axum::middleware::Next;
use axum::http::Request;

pub async fn validate_request_size<B>(
    req: Request<B>,
    next: Next,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Validate content-length header
    if let Some(header) = req.headers().get(axum::http::header::CONTENT_LENGTH) {
        if let Ok(size_str) = header.to_str() {
            if let Ok(size) = size_str.parse::<u64>() {
                let max_size = 50 * 1024 * 1024;  // 50 MB
                if size > max_size {
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!("Request too large: {} bytes (max: {})", size, max_size),
                    ));
                }
            }
        }
    }
    Ok(next.run(req).await)
}
```

**Option C: Add Request Count Limit Per Job**
```rust
// In JobRequest validation:
const MAX_FILES: usize = 100;
const MAX_TESTCASES: usize = 1000;

fn validate_job_request(req: &JobRequest) -> Result<(), (StatusCode, String)> {
    if req.files.len() > MAX_FILES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Too many files: {} (max: {})", req.files.len(), MAX_FILES),
        ));
    }

    let total_size = req.files.iter().map(|f| f.content.len()).sum::<usize>();
    const MAX_TOTAL_SIZE: usize = 50 * 1024 * 1024;  // 50 MB

    if total_size > MAX_TOTAL_SIZE {
        return Err((
            StatusCode::REQUEST_ENTITY_TOO_LARGE,
            format!("Total file size too large: {} bytes", total_size),
        ));
    }

    if let Some(testcases) = &req.testcases {
        if testcases.len() > MAX_TESTCASES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Too many testcases: {} (max: {})", testcases.len(), MAX_TESTCASES),
            ));
        }
    }

    Ok(())
}
```

**Effort:** 
- Option A: 5 minutes
- Option B: 20 minutes
- Option C: 15 minutes

**Recommendation:** Use all three options for defense-in-depth.

---

## MEDIUM PRIORITY ISSUES 🟡

### 5. Timeout Precision Loss

**File:** [apps/jet-server/src/sandbox/container.rs](apps/jet-server/src/sandbox/container.rs#L233)

**Severity:** 🟡 MEDIUM

**Description:**
Integer division truncates timeout values, causing inaccuracy:

```rust
let timeout_secs = std::cmp::max(1, timeout_ms / 1000);
// 500ms → 0 → 1s (forced to 1 second!)
// 1500ms → 1s (should be 2s)
```

**Impact:** MEDIUM
- A 500ms timeout becomes 1 second (200% longer!)
- Competitive programming judges rely on accurate timeout
- Fair evaluation of solutions is compromised

**Fix:**
```rust
// Round UP instead of truncating
let timeout_secs = std::cmp::max(1, (timeout_ms + 999) / 1000);
// Or use proper ceiling:
let timeout_secs = std::cmp::max(1, timeout_ms.div_ceil(1000));
```

**Effort:** 2 minutes

**Testing:**
```rust
#[test]
fn timeout_rounding_is_correct() {
    assert_eq!(round_up(500), 1);     // 500ms → 1s (min)
    assert_eq!(round_up(1000), 1);    // 1000ms → 1s
    assert_eq!(round_up(1500), 2);    // 1500ms → 2s
    assert_eq!(round_up(3000), 3);    // 3000ms → 3s
}
```

---

### 6. CPU Time Calculation Precision Loss

**File:** [apps/jet-server/src/sandbox/container.rs](apps/jet-server/src/sandbox/container.rs#L293-L295)

**Severity:** 🟡 MEDIUM

**Description:**
Float to u64 conversion can lose sub-millisecond precision:

```rust
cpu_time = Some((
    (rusage.user_time.as_secs_f64() + rusage.system_time.as_secs_f64()) * 1000.0
) as u64);
```

**Recommendation:**
Use nanosecond precision throughout:

```rust
let user_ns = rusage.user_time.as_nanos();
let system_ns = rusage.system_time.as_nanos();
let total_ns = user_ns + system_ns;
cpu_time = Some((total_ns / 1_000_000) as u64);  // Convert to ms
```

**Effort:** 5 minutes

---

### 7. String Parsing for Status Detection

**File:** [apps/jet-server/src/sandbox/container.rs](apps/jet-server/src/sandbox/container.rs#L284-L290)

**Severity:** 🟡 MEDIUM

**Description:**
Fragile string-based status detection depends on error message format:

```rust
let reason_lc = status.reason.to_lowercase();

if reason_lc.contains("timed out")
    || internal_code == 128 + libc::SIGKILL
    || process_exit_code == Some(128 + libc::SIGKILL)
{
    StageStatus::TimeLimitExceeded
}
```

**Risk:** If Hakoniwa changes error message format, status detection fails

**Fix:**
Check if Hakoniwa provides structured status enums. If not:
```rust
fn detect_stage_status(
    is_success: bool,
    exit_code: Option<i32>,
    signal: Option<i32>,
    reason: &str,
) -> StageStatus {
    if is_success {
        return StageStatus::Success;
    }

    // Check signals first (structured)
    if let Some(sig) = signal {
        if sig == libc::SIGKILL || sig == libc::SIGTERM {
            return StageStatus::TimeLimitExceeded;
        }
        if sig == libc::SIGXFSZ {
            return StageStatus::OutputLimitExceeded;
        }
    }

    // Check exit code (structured)
    if let Some(code) = exit_code {
        if code == 128 + libc::SIGKILL {
            return StageStatus::TimeLimitExceeded;
        }
        if code == 128 + libc::SIGXFSZ {
            return StageStatus::OutputLimitExceeded;
        }
    }

    // Fallback to string parsing
    let reason_lc = reason.to_lowercase();
    if reason_lc.contains("out of memory") || reason_lc.contains("cannot allocate memory") {
        return StageStatus::MemoryLimitExceeded;
    }

    StageStatus::RuntimeError
}
```

**Effort:** 15 minutes

---

### 8. Hardcoded JVM Flags

**File:** [apps/jet-server/src/worker/evaluator.rs](apps/jet-server/src/worker/evaluator.rs#L13-L29)

**Severity:** 🟡 MEDIUM

**Description:**
JVM flags are hardcoded constants, preventing deployment-specific tuning:

```rust
const JAVA_COMPILE_JVM_FLAGS: &[&str] = &[
    "-Xms16m", "-Xmx256m",
    "-XX:MaxMetaspaceSize=64m",
    "-XX:CompressedClassSpaceSize=32m",
    "-XX:ReservedCodeCacheSize=32m",
    "-XX:+UseSerialGC",
    "-Xss256k",
];
```

**Issues:**
- Can't adjust for different server sizes
- Can't optimize for specific workloads
- Requires code change for tuning
- Not documented in manifest

**Fix Option A: Add to Manifest**
```yaml
# java-21.yaml
language: java
version: 21.0.1
compile_jvm_flags:
  - -Xms16m
  - -Xmx256m
  - -XX:MaxMetaspaceSize=64m
run_jvm_flags:
  - -Xms8m
  - -Xmx64m
```

Then update `ExecutionTemplate`:
```rust
pub struct ExecutionTemplate {
    pub command: String,
    pub args: Option<Vec<String>>,
    pub jvm_flags: Option<Vec<String>>,  // Add this
}
```

**Fix Option B: Add to JetConfig**
```rust
pub struct JetConfig {
    // ... existing ...
    pub java_compile_jvm_flags: Vec<String>,
    pub java_run_jvm_flags: Vec<String>,
}
```

**Effort:** 
- Option A: 1 hour (update updater, manifest struct, evaluator)
- Option B: 30 minutes (config loading, passing through context)

**Recommendation:** Option A (manifest-based) for per-language customization

---

## LOW PRIORITY ISSUES 🟢

### 9. Missing Test Coverage for Failure Scenarios

**Severity:** 🟢 LOW

**Description:**
PLAN requirement: "Write tests for everything we build. Make sure all the tests pass before committing. The tests should not only test the working, but failure points too. Cover points like checking if TLE, MLE, OLE, Fork Bomb Prevention, Compile and Runtime Errors, all are correctly detected and parsed."

**Current Status:**
- ✅ 39 tests passing
- ✅ Basic functionality tested
- ❌ Memory Limit Exceeded not tested
- ❌ Output Limit Exceeded not tested
- ❌ Compilation errors not tested
- ❌ Fork bomb prevention not tested
- ❌ Concurrent job execution not tested
- ❌ Security boundary testing not done

**Tests To Add:**
```rust
#[test]
fn sandbox_memory_limit_exceeded() {
    // Allocate >10MB when limit is 10MB
    // Should return MemoryLimitExceeded status
}

#[test]
fn sandbox_output_limit_exceeded() {
    // Write >1MB when limit is 1MB
    // Should return OutputLimitExceeded status
}

#[test]
fn sandbox_compile_error_detected() {
    // Submit invalid Java code
    // Should return CompilationError status with stderr populated
}

#[test]
fn sandbox_cannot_escape_workspace() {
    // Try to read /etc/passwd from within sandbox
    // Should fail or return permission denied
}

#[test]
fn concurrent_jobs_isolated() {
    // Run two jobs simultaneously
    // Verify they don't interfere with each other
}

#[test]
fn fork_bomb_prevented() {
    // Try to fork many processes
    // Should be limited by pid_limit resource
}
```

**Effort:** 4 hours (write and debug tests)

**Priority:** Lower than security fixes but important for reliability

---

### 10. Unused Dead Code: `SandboxProfile::strict()`

**File:** [apps/jet-server/src/sandbox/container.rs](apps/jet-server/src/sandbox/container.rs#L23)

**Severity:** 🟢 LOW

**Description:**
`SandboxProfile::strict()` is defined but never called, flagged as dead code.

**Fix:**
- Use it (see issue #2 above), or
- Delete it if `portable` is the intended default

**Effort:** 1 minute (after fixing issue #2)

---

### 11. No Workspace Cleanup After Job Execution

**File:** [apps/jet-server/src/worker/runner.rs](apps/jet-server/src/worker/runner.rs#L110-L170)

**Severity:** 🟢 LOW

**Description:**
After a job completes, the workspace directory is never cleaned up:

```rust
let workspace_dir = data.runtime_install_dir.join("jobs").join(&job.id);
fs::create_dir_all(&workspace_dir)
    .await
    .map_err(|source| std::io::Error::other(source.to_string()))?;

// ... job executes ...
// No cleanup! Workspace stays on disk, accumulates over time
```

**Impact:** 
- Disk space gradually fills up
- Could eventually cause system to run out of space
- Old job data remains accessible

**Fix:**
```rust
use tempfile::TempDir;

// Use self-cleaning temp directory
let workspace_dir = TempDir::new()
    .map_err(|source| std::io::Error::other(source.to_string()))?;

// ... job executes using workspace_dir.path() ...

// Automatically cleaned up when workspace_dir is dropped
```

Or explicit cleanup:
```rust
// After job completes
fs::remove_dir_all(&workspace_dir)
    .await
    .map_err(|source| std::io::Error::other(source.to_string()))?;
```

**Effort:** 15 minutes

---

### 12. No Logging / Observability

**Severity:** 🟢 LOW

**Description:**
No logging infrastructure for debugging, monitoring, or auditing:
- Job lifecycle events not logged
- Worker errors printed to stderr
- No structured logging
- No metrics collection beyond per-job data

**Recommendation:**
```rust
use tracing::{info, warn, error, span, Level};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    // ... setup code ...

    let span = span!(Level::INFO, "job_execution", job_id = ?job.id);
    let _enter = span.enter();
    info!("job starting: language={} version={}", job.language, job.version);
    // ...
    info!("job completed: status={:?}", result.status);
}
```

**Effort:** 2 hours

---

### 13. No Graceful Shutdown

**Severity:** 🟢 LOW

**Description:**
Server doesn't handle graceful shutdown; kills in-flight jobs:

```rust
// apps/jet-server/src/main.rs
axum::serve(listener, app).await?;
// Quits immediately on signal, doesn't wait for workers
```

**Fix:**
```rust
tokio::select! {
    _ = tokio::signal::ctrl_c() => {
        info!("shutdown signal received, stopping gracefully");
        // Worker should respect shutdown channel
        worker_shutdown_tx.send(()).ok();
        // Wait for worker to drain queue
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    result = axum::serve(listener, app) => {
        return result.map_err(Into::into);
    }
}
```

**Effort:** 1 hour

---

## Summary Table

| # | Issue | Severity | Component | Effort | Status |
|---|-------|----------|-----------|--------|--------|
| 1 | Missing namespace unsharing | 🔴 CRITICAL | sandbox/container.rs | 5m | ❌ Not Fixed |
| 2 | Security profiles not enforced | 🔴 CRITICAL | sandbox/evaluator | 5m-30m | ❌ Not Fixed |
| 3 | Path traversal in archives | 🔴 CRITICAL | jet-pack/archive.rs | 20m | ❌ Not Fixed |
| 4 | No request validation/limits | 🔴 CRITICAL | jet-server/api.rs | 5m-15m | ❌ Not Fixed |
| 5 | Timeout precision loss | 🟡 MEDIUM | sandbox/container.rs | 2m | ❌ Not Fixed |
| 6 | CPU time precision loss | 🟡 MEDIUM | sandbox/container.rs | 5m | ❌ Not Fixed |
| 7 | String-based status detection | 🟡 MEDIUM | sandbox/container.rs | 15m | ❌ Not Fixed |
| 8 | Hardcoded JVM flags | 🟡 MEDIUM | worker/evaluator | 30m-1h | ❌ Not Fixed |
| 9 | Missing test coverage | 🟡 MEDIUM | all | 4h | ❌ Not Fixed |
| 10 | Dead code: `strict()` | 🟢 LOW | sandbox/container.rs | 1m | ❌ Not Fixed |
| 11 | No workspace cleanup | 🟢 LOW | worker/runner | 15m | ❌ Not Fixed |
| 12 | No logging/observability | 🟢 LOW | all | 2h | ❌ Not Fixed |
| 13 | No graceful shutdown | 🟢 LOW | jet-server/main.rs | 1h | ❌ Not Fixed |

---

## Recommended Fix Priority

### Phase 1: Critical Security Fixes (MUST DO - 45 minutes)
1. **Add missing namespaces** (User, Mount, PID) - 5 min
2. **Enable security profiles** - 5 min
3. **Add request validation** - 15 min
4. **Fix path traversal** - 20 min

**Status Check:** After Phase 1, run full test suite and security audit.

### Phase 2: Reliability Fixes (SHOULD DO - 30 minutes)
5. Fix timeout precision - 2 min
6. Fix CPU time precision - 5 min
7. Remove dead code - 1 min
8. Add workspace cleanup - 15 min
9. Better status detection - 15 min

### Phase 3: Quality Improvements (NICE TO HAVE - 6+ hours)
10. Add comprehensive test coverage - 4 hours
11. Add logging/observability - 2 hours
12. Add graceful shutdown - 1 hour
13. Manifest-based JVM flags - 1 hour

---

## Production Readiness Checklist

After fixes are applied:

- [ ] All critical issues resolved (Phase 1)
- [ ] All tests passing (39 + new tests)
- [ ] Manual security testing completed
  - [ ] Namespace isolation verified
  - [ ] Filesystem boundary respected
  - [ ] Syscall filtering active (if strict mode)
  - [ ] Cross-job isolation verified
- [ ] Load testing completed (concurrent jobs)
- [ ] Timeout accuracy verified
- [ ] Memory limit enforcement verified
- [ ] Output limit enforcement verified
- [ ] Documentation updated
- [ ] Security review by external party (recommended)

---

## Deployment Recommendations

### Before Going to Production

1. **Enable Strict Security Profile** in configuration
2. **Run comprehensive test suite** including new failure scenario tests
3. **Deploy to staging** and perform:
   - Load testing with 100+ concurrent jobs
   - Fault injection testing (simulate timeouts, OOM)
   - Security boundary testing
4. **Monitor resource usage** (memory, CPU, disk)
5. **Set up alerting** for:
   - Job failures
   - Resource exhaustion
   - API errors
6. **Document operational procedures**:
   - How to handle sandbox escape attempts
   - How to monitor worker health
   - How to recover from failures

### Runtime Configuration (Production)

```toml
# .env or config file
JET_SANDBOX_PROFILE=strict           # Use strict security profile
JET_SERVER_PORT=4000
JET_REDIS_URL=redis://redis:6379     # Use Redis, not localhost
JET_RUNTIME_DIR=/opt/jet/runtimes    # Use dedicated volume
JET_MAX_REQUEST_SIZE=50M              # 50 MB limit
JET_MAX_FILES_PER_JOB=100
JET_MAX_TESTCASES_PER_JOB=1000
```

---

## References

- [PLAN.md](PLAN.md) - Original implementation plan (Phase 3 security details)
- [Hakoniwa Documentation](https://github.com/aembke/hakoniwa) - Sandbox library
- [Linux Namespaces](https://man7.org/linux/man-pages/man7/namespaces.7.html) - Isolation mechanisms
- [Landlock](https://docs.kernel.org/userspace-api/landlock.html) - Filesystem access control
- [Seccomp](https://man7.org/linux/man-pages/man2/seccomp.2.html) - System call filtering

