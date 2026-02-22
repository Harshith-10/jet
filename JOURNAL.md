# Jet Development Journal

## 2026-02-22

### Why this file exists
This is my running engineering diary for Jet so we do not lose feature intent, constraints, and follow-up ideas between phases.

### Current Focus
- Phase 2: Package & Version Management in `jet-pack`
- Keep APIs small and composable so `jet-server` and `jet-cli` can consume them directly.
- Enforce deterministic tests for both success and failure paths.

### Phase 2 Build Checklist
- [x] Manifest model and YAML parser
- [x] Runtime manifest directory scanner
- [x] Version fragment resolution map generator
- [x] O(1) lookup abstraction with in-memory backend
- [ ] Optional Redis-backed lookup backend (next increment)
- [x] Download utility (manifest/runtime assets)
- [x] Archive extraction utility
- [x] Package install/update management helpers
- [x] Tests for happy path + invalid YAML + missing files + unsupported archive

### Design Notes
- Keep version resolution language-scoped: `language:fragment -> full_version`.
- Treat exact full version as valid fragment too.
- Scanner accepts `.yaml` and `.yml` manifests.
- Default approach for now: in-memory map with trait abstraction; Redis backend can plug in without changing callers.

### Risks / Reminders
- Avoid introducing async runtime coupling in `jet-pack` unless required.
- Keep install/update functions testable without internet by supporting `file://` URLs.
- Ensure path handling is explicit and cross-platform where possible.

### Completed Today
- Added a real `jet-pack` library with modules: `manifest`, `resolver`, `downloader`, `archive`, `manager`, and `error`.
- Implemented version map generation using semantic-version ordering so short tags resolve to latest patch.
- Added in-memory version resolver backend with O(1) key lookups.
- Added download support for both `file://` and HTTP(S) URLs.
- Added extraction support for `.tar.gz` and `.zip` archives.
- Added package management helpers to scan manifests, update manifests from sources, and install runtime archives by architecture.
- Added comprehensive unit tests for success and failure paths.

### Next Phase 2 Increments
- Add checksum verification (`sha256`) during runtime archive install.
- Add repository release polling helper for latest manifest/runtime discovery.

### Phase 2 Update (Latest)
- Completed Redis-backed version store in `jet-pack` resolver layer.
- `VersionStore` now returns `JetPackResult` so backend failures are surfaced (important for Redis connectivity/runtime errors).
- Added `RedisVersionStore` with hash-key persistence (`DEL` + `HSET` write path, `HGET` read path).
- Exported Redis store from crate API for server-side consumption.
- Decision confirmed: skip SHA256 verification for installs in this private project workflow.

### Remember
- Make worker/job submission use resolver.resolve(...) against Redis at runtime in jet-server once we add API/queue flow.

### Updater Architecture (Added)
- Added trait-first updater model in `jet-pack` so each language keeps its own release parsing logic:
	- `RuntimeUpdater` trait with `fetch_updated_manifests()`
	- `UpdatedManifest` output model (`file_name` + normalized `RuntimeManifest`)
- Added `JavaCorrettoUpdater` with Corretto-specific extraction logic from GitHub release payloads.
- Added `PythonStandaloneUpdater` with python-build-standalone asset parsing and latest-per-major.minor selection.
- Added `PackageManager::update_manifests_with_updater(...)` to persist updater outputs as YAML in a uniform path.

### Why this helps
- Future language updaters can be plugged in without changing manager flow.
- Runtime-specific complexity stays local to each updater implementation.
- The rest of the system consumes a single normalized manifest model.

### Phase 3 Update (Latest)
- Added Hakoniwa-based sandbox module in `jet-server` with namespace isolation and controlled mounts.
- Implemented resource guardrails via `setrlimit` and optional cgroups configuration.
- Implemented optional Landlock and Seccomp policy enforcement with an allowlist strategy.
- Added `SandboxProfile` modes:
	- `strict()` for hardened worker execution.
	- `portable()` for deterministic local test execution where some kernel capabilities may not be available.
- Added telemetry extraction (runtime/cpu/memory) and stage-status mapping to `StageStatus` values (`SUCCESS`, `TIME_LIMIT_EXCEEDED`, `MEMORY_LIMIT_EXCEEDED`, `OUTPUT_LIMIT_EXCEEDED`, runtime error fallback).
- Added worker evaluator skeleton that runs compile and execute templates from manifests through sandboxed execution.
- Added tests in `jet-server` for success and timeout behavior; full `cargo test -p jet-server` passes.
- Important mount fix from Hakoniwa docs: avoid mixing `rootfs("/")` with explicit bind mounts for this flow to prevent mount/symlink conflicts.

### Next (Phase 4 Start)
- Wire REST job submission API to enqueue requests.
- Resolve runtime versions via Redis-backed resolver during submission.
- Connect queue consumer worker to evaluator execution path.

### Phase 4 Update (In Progress)
- Added async server runtime in `jet-server` using `tokio` + `axum`.
- Added API module with:
	- `GET /health` basic liveness.
	- `POST /jobs` submission endpoint.
- Added `GET /jobs/:id` endpoint for job state polling.
- Submission flow now resolves `language:version_fragment` through Redis-backed `VersionResolver` before enqueueing.
- Added Redis queue integration via `apalis-redis`:
	- Producer pushes normalized jobs after version resolution.
	- Worker consumes queued jobs and executes evaluator pipeline.
- Added `QueuedJob` model to persist normalized payload (`id`, `language`, exact `version`, full request).
- Added Redis-backed `JobStateRecord` persistence for lifecycle states:
	- `queued` on submission.
	- `running` when worker starts execution.
	- `completed` with full `JobResult` payload when evaluation succeeds.
	- `failed` with error message when evaluation fails.
- Added API polling tests using embedded `mini-redis`:
	- found job id -> `200 OK` with decoded job state.
	- missing job id -> `404 NOT_FOUND`.
- Hardened worker runtime checks before sandbox execution:
	- validates manifest has runtime archive entry for normalized host architecture.
	- validates installed runtime root exists at `<runtime_install_dir>/<language>/<version>/root`.
- Main server startup now runs both:
	- HTTP API listener.
	- Background worker loop.
- Validation:
	- `cargo check -p jet-server` passes.
	- `cargo test -p jet-server` passes.

### Phase 4 Next Steps
- Improve worker runtime path resolution with architecture/runtime checks before execute.
- Add API integration tests for submit + poll flow (requires controllable Redis in test harness).
