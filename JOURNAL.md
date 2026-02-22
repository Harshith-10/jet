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
