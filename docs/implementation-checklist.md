# px Spec Implementation Checklist

Status legend: [x] Done · [~] In Progress · [ ] Not Started · [!] Blocked
Reminder: use [ ] for items that simply haven’t been built yet, and reserve [!] for work blocked on external dependencies.

## Projects & Environments

- [x] **Project discovery & ownership** – Matches spec 2.1/2.2: project roots are detected via `px.lock` or `[tool.px]`, and `px init` only edits `[project]`/`[tool.px]` (`px_project/src/snapshot.rs:56`, `px-project/src/init.rs:15`).
- [x] **Self-consistency guarantees** – All mutating commands snapshot `pyproject.toml`/`px.lock`, restore on failure, and require `px sync` to rehydrate envs (`px-core/src/commands/project.rs:124`, `px-core/src/commands/project.rs:611`).
- [!] **px-managed runtimes/envs** – Spec 2.2/3.2/5.1 call for `.px/envs/...` tied to lock/runtime, but current flows just call the process Python and rely on `.px/site/px.pth`; this is blocked on designing the runtime registry + materialization story.
- [x] **Artifacts live in `dist/`** – `px build` now defaults to `project_root/dist`, and `px publish` reads from the same directory per spec 2.3/4.4 (`crates/px-core/src/commands/output.rs:64-197`, `crates/px-cli/src/main.rs:820`).

## Core Workflow Commands

- [x] **`px add` / `px remove` / `px sync` / `px update` / `px status` / `px migrate`** – Implement Section 5 contracts: mutate `[project].dependencies`, resolve, rewrite `px.lock`, refresh env metadata, and report drift (`px-core/src/commands/project.rs:124-460`, `px-core/src/commands/migrate.rs`).
- [x] **`px run` / `px test`** – Enforce dev vs `--frozen` semantics, auto-sync in dev, refuse in CI, and attach missing-import hints per spec 5.6/5.7 & 8.2 (`px-core/src/commands/workflow.rs`, `px-core/src/traceback.rs`).
- [x] **`px fmt`/`px lint`** – Respect CI guard, read `[tool.px.fmt|lint]`, default to Ruff, and operate inside the px environment (`px-core/src/commands/quality.rs`).
- [x] **Default script lookup** – `px run` now falls back to `[tool.px.scripts]` when `[project].scripts` is absent, matching spec 5.6 (`crates/px-core/src/commands/workflow.rs:160-520`).
- [x] **`px fmt` / `px lint` tool installation UX** – Both commands now accept `--frozen`/`CI=1` guards and emit actionable `px add --group dev …` suggestions instead of mutating dependencies when tools are missing (`crates/px-core/src/commands/quality.rs:21-360`, `crates/px-cli/src/main.rs:660-969`, `docs/spec.md:472-485`).
- [x] **`px status` output** – Status details now include project name/root plus the active runtime path/version per spec 5.9 (`crates/px-core/src/commands/project.rs:262-360`).

## Tools & Runtimes

- [!] **Global tool lifecycle** – Spec 1.3/4.2/6 require `px tool install/run/list/remove/upgrade` with isolated CAS envs. Implementation is blocked on the forthcoming CAS-backed tool store and UX design (`crates/px-cli/src/main.rs:683-769`).
- [!] **Runtime management (`px python …`)** – Section 4.3 & 7 define runtime discovery, install, selection, and failure messaging. Work is blocked until the runtime installer/registry story is defined beyond `PX_RUNTIME_PYTHON`.

## Distribution & Introspection

- [!] **`px why`** – Intended spec 4.5 introspection command is stubbed out as “upcoming”; progress is blocked on dependency provenance plumbing in px-core (`crates/px-cli/src/main.rs:428`).
- [x] **CLI surface mismatches** – Removed the `px lock`, `px workspace`, and legacy `px cache`/`px env` aliases so only the spec-authorized commands remain visible; helper commands now live solely under `px debug …` (`crates/px-cli/src/main.rs:26-200,660-865`, `crates/px-cli/tests/prefetch_workspace.rs`).

## Error & Output Model

- [x] **Heuristics for missing imports & drift** – Implemented via structured tracebacks and `InstallUserError` details (spec 8.2) (`px-core/src/traceback.rs`, `px-core/src/lib.rs:1868-2040`).
- [x] **PX-styled envelopes (`PX123 / Why / Fix`)** – Human output now emits PX codes, “Why” bullets, and “Fix” bullets with colored headers plus post-summary tracebacks per spec 8.1 (`crates/px-cli/src/main.rs:70-230`, `crates/px-cli/src/style.rs:11-62`).
- [x] **Resolver error UX** – Resolver failures now emit structured `reason`/`issues`/`hint` details so CLI surfaces actionable “Why / Fix” guidance per spec 5.2/5.3 (`crates/px-core/src/lib.rs:833-924`, `crates/px-cli/src/main.rs:95-374`).

## Next Actions

1. Design and implement px-managed runtimes/env directories plus the `px python` surface so environments are deterministic and separate from the host interpreter.
2. Bring the CLI surface in line with the spec (remove or hide disallowed commands, add the tool lifecycle and `px why`).
3. Align artifacts/error UX with the spec (write to `dist/`, emit PX error envelopes, implement `[tool.px].scripts` + frozen modes for fmt/lint).
