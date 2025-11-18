# px Spec Implementation Checklist

Status legend: [x] Done · [~] In Progress · [ ] Not Started · [!] Blocked
Reminder: use [ ] for items that simply haven’t been built yet, and reserve [!] for work blocked on external dependencies.

## Projects & Environments

- [x] **Project discovery & ownership** – Matches spec 2.1/2.2: project roots are detected via `px.lock` or `[tool.px]`, and `px init` only edits `[project]`/`[tool.px]` (`px_project/src/snapshot.rs:56`, `px-project/src/init.rs:15`).
- [x] **Self-consistency guarantees** – All mutating commands snapshot `pyproject.toml`/`px.lock`, restore on failure, and require `px sync` to rehydrate envs (`px-core/src/commands/project.rs:124`, `px-core/src/commands/project.rs:611`).
- [x] **px-managed runtimes/envs** – `refresh_project_site` now materializes environments under `.px/envs/<env-id>` based on the current lock hash + runtime (`crates/px-core/src/lib.rs:693`), records metadata in `.px/state.json`, and `python_context` builds PYTHONPATH from that state so commands refuse to run when the env is missing or out-of-date (`crates/px-core/src/lib.rs:2042`).
- [x] **Artifacts live in `dist/`** – `px build` now defaults to `project_root/dist`, and `px publish` reads from the same directory per spec 2.3/4.4 (`crates/px-core/src/commands/output.rs:64-197`, `crates/px-cli/src/main.rs:820`).

## Core Workflow Commands

- [x] **`px add` / `px remove` / `px sync` / `px update` / `px status` / `px migrate`** – Implement Section 5 contracts: mutate `[project].dependencies`, resolve, rewrite `px.lock`, refresh env metadata, and report drift (`px-core/src/commands/project.rs:124-460`, `px-core/src/commands/migrate.rs`).
- [x] **`px run` / `px test`** – Enforce dev vs `--frozen` semantics, auto-sync in dev, refuse in CI, and attach missing-import hints per spec 5.6/5.7 & 8.2 (`px-core/src/commands/workflow.rs`, `px-core/src/traceback.rs`).
- [x] **`px fmt`/`px lint`** – Respect CI guard, read `[tool.px.fmt|lint]`, default to Ruff, and operate inside the px environment (`px-core/src/commands/quality.rs`).
- [x] **Default script lookup** – `px run` now falls back to `[tool.px.scripts]` when `[project].scripts` is absent, matching spec 5.6 (`crates/px-core/src/commands/workflow.rs:160-520`).
- [x] **`px fmt` / `px lint` tool installation UX** – Both commands now accept `--frozen`/`CI=1` guards and emit actionable `px add --group dev …` suggestions instead of mutating dependencies when tools are missing (`crates/px-core/src/commands/quality.rs:21-360`, `crates/px-cli/src/main.rs:660-969`, `docs/spec.md:472-485`).
- [x] **`px status` output** – Status details now include project name/root plus the active runtime path/version per spec 5.9 (`crates/px-core/src/commands/project.rs:262-360`).

## Tools & Runtimes

- [x] **Global tool lifecycle** – Added `px tool install/run/list/remove/upgrade`, which manages tools under `~/.px/tools/<name>` with dedicated `pyproject.toml`, `px.lock`, `.px/site`, and metadata (`crates/px-core/src/commands/tool.rs`, `crates/px-cli/src/main.rs:760-980`). Each install resolves pins via the shared resolver, binds to a runtime from `px python`, and `px tool run` executes inside the cached CAS-backed env with drift/runtimes enforced.
- [x] **Runtime management (`px python …`)** – Added a JSON-backed runtime registry plus `px python list/install/use/info` commands (`crates/px-core/src/runtime.rs`, `crates/px-core/src/commands/python.rs`, `crates/px-cli/src/main.rs:760-930`). Projects now honor `[tool.px].python` and fall back to registry runtimes before touching the host interpreter.

## Distribution & Introspection

- [x] **`px why`** – Added a top-level `px why` that inspects the project env metadata and reports direct or transitive dependency chains (`crates/px-core/src/commands/project.rs:512-890`, `crates/px-cli/src/main.rs:658-1110`).
- [x] **CLI surface mismatches** – Removed the `px lock`, `px workspace`, and legacy `px cache`/`px env` aliases so only the spec-authorized commands remain visible; helper commands now live solely under `px debug …` (`crates/px-cli/src/main.rs:26-200,660-865`, `crates/px-cli/tests/prefetch_workspace.rs`).

## Error & Output Model

- [x] **Heuristics for missing imports & drift** – Implemented via structured tracebacks and `InstallUserError` details (spec 8.2) (`px-core/src/traceback.rs`, `px-core/src/lib.rs:1868-2040`).
- [x] **PX-styled envelopes (`PX123 / Why / Fix`)** – Human output now emits PX codes, “Why” bullets, and “Fix” bullets with colored headers plus post-summary tracebacks per spec 8.1 (`crates/px-cli/src/main.rs:70-230`, `crates/px-cli/src/style.rs:11-62`).
- [x] **Resolver error UX** – Resolver failures now emit structured `reason`/`issues`/`hint` details so CLI surfaces actionable “Why / Fix” guidance per spec 5.2/5.3 (`crates/px-core/src/lib.rs:833-924`, `crates/px-cli/src/main.rs:95-374`).

## Next Actions

1. Generate console-script shims for installed tools so `px tool run` can invoke entry points that don’t expose `python -m` modules, and cache binary stubs for faster dispatch.
2. Share tool envs across machines via a CAS-aware tool store (dedupe downloads, support multi-platform locks) and add richer upgrade controls (e.g., `px tool upgrade black==X`).
3. Expand automated tests/fixtures to cover tool lifecycle flows without requiring PyPI (local index or wheel fixtures) to ensure CI coverage.
