# Architecture

This document highlights the layout, responsibilities, and Phase A scope that
keep `px` focused on a deterministic, cache-backed Python toolchain. It aligns
with `docs/spec.md` (sections 2–14) and `docs/requirements.md` (Phase A
acceptance, backlog, and dependencies).

## Workspace Layout

```text
px/                             # root workspace (Cargo)
├── Cargo.toml                  # workspace manifest
├── crates/
│   ├── px-cli                  # thin entry point, argument parsing, UX layer
│   ├── px-core                 # shared context, command registry, workspace model
│   ├── px-project              # layout validation + `.px/` bootstrap
│   ├── px-lockfile             # serializer/parser for `px.lock`
│   ├── px-resolver             # resolver (markers/extras, PyPI client)
│   ├── px-store                # global CAS store + metadata layout
│   ├── px-python               # interpreter helpers + shim generation
│   ├── px-runtime              # command execution, bootstrap runner, env injection
│   └── px-cache                # CAS cache manager, pruning, stats, policy hooks
├── docs/                       # specs + supporting documentation
├── fixtures/                   # sample projects (e.g., `sample_px_app`)
└── tests/                      # integration tests (cargo-driven)
```

- Every crate integrates through shared types exposed by `px-core`.
- `px.lock`, `pyproject.toml`, and `.px/` stay per-project with enforced
  layout (see `docs/spec.md` §2–4).
- `.px/` remains hidden; it stores `site/px.pth`, shimmed binaries, derived
  metadata, and the runner bootstrap regenerated whenever `px sync` runs.

## Crate Responsibilities

- **px-cli** – parses commands (init/add/install/run/test/fmt/lint/
  build/publish/cache/env/tidy), maps them to `px-core`, wires flags, and emits
  terse errors.
- **px-core** – shared context, workspace discovery, feature registry,
  telemetry stubs, and command dispatch used by the CLI.
- **px-project** – validates `src/`, `tests/`, `pyproject.toml`, `.px/`, fills
  `.px/env.json`, and manages `[tool.px.scripts]` plus extras.
- **px-lockfile** – serializes/deserializes `px.lock` (single-target Phase A),
  enforces `version, name, filename, sha256, tags, parents`, and exposes
  helpers for `--frozen` checks.
- **px-resolver** – implements PEP 440/508 semantics, marker evaluation,
  deterministic backtracking, and produces graphs consumed by `px-lockfile`
  and `px-store`.
- **px-store** – global CAS (`~/.cache/px/store/<algo>/<hash>/`), downloads,
  wheel extraction, `meta.json`, shim generation, cache pruning, and hash
  verification.
- **px-python** – bootstrap helpers for the `px run` shim, injects `.px/site`
  via `site.addsitedir`, enforces `-s`/`PYTHONNOUSERSITE=1`, and exposes `px env
  python` for IDE/CI.
- **px-runtime** – executes commands with `.px/site` + `src/` precedence,
  manages `px run`, wraps subprocesses for `fmt`, `lint`, `test`, `build`, and
  surfaces consistent exit codes.
- **px-cache** – implements `px cache {path,stats,prune}`, cache policies, and
  hooks shared with `px-store`.

## Feature Flags (Phase A)

- `cli/audit-logs` – enables structured logging for diagnostics (off by
  default, targeted for Phase B+).
- `project/pep582-compat` – toggles optional `px env --mode pep582`
  materialization (default off).
- `store/local-only` – forces the in-process CAS backend; placeholder for remote
  backends introduced later.
- `resolver/fast-markers` – selects the deterministic marker evaluator to keep
  installs reproducible even before multi-target locks.
- `runtime/isolated-build` – keeps PEP 517 builds hermetic and caches wheels
  per target per `docs/spec.md` §6.

Defaults favor the smallest viable binary. Future phases (workspaces,
interpreters, policy hooks) extend these flags rather than rewiring crates.

## Error Model

- Centralized `px-core::Error` (via `thiserror` + `miette`) wraps context such
  as `command`, `workspace`, and `path`.
- CLI reporting mirrors `docs/spec.md` §5/§11: headline + explicit next step
  (e.g., “Lockfile drift detected → run `px sync` or `px update <dep>`”).
- Runtime/resolver/store emit structured metadata for machine consumption
  (`--json` planned in Phase B) and log spans (`resolve`, `download`, `build`,
  `install`) using `tracing`.
- Commands either succeed or leave `.px/site` in a deterministic state;
  `px cache prune` never corrupts CAS; `px sync` rolls back partial work.
- Later phases layer telemetry/policy hooks without changing the success/error
  contract.

## Phase A Command Groups

- **Project scaffolding** – `px init` creates the enforced layout
  (`pyproject`, `.px/`, lock template) via `px-project`, `px-cli`, and
  `px-lockfile`.
- **Dependency lifecycle** – `px add`, `px sync`, `px update`, `px remove`
  resolve specs, update CAS, regenerate `.px/site`, and enforce `--frozen`
  through `px-resolver`, `px-store`, `px-lockfile`, and `px-project`.
- **Execution** – `px run [module|script|entry]` bootstraps the runner, injects
  `.px/site`, prioritizes `src/`, and forwards args using `px-runtime`,
  `px-python`, and `px-project`.
- **Quality tooling** – `px test`, `px fmt`, `px lint`, `px tidy` invoke
  managed dev tools (`pytest`, `ruff`), tidy metadata, and clean `.px` via
  `px-runtime`, `px-project`, and `px-cache`.
- **Delivery** – `px build [sdist|wheel|both]` and `px publish` handle PEP 517
  builds, wheel caches, and registry uploads using `px-runtime`, `px-store`,
  and `px-lockfile`.
- **Support** – `px cache {path,stats,prune}` plus `px env {python,info,paths}`
  inspect CAS, reveal the resolved cache root, and surface interpreter+path
  details through `px-cache`, `px-python`, and `px-store`. The CLI exposes both
  grouped forms (`px infra env`) and top-level aliases (`px env …`).

### Env & Cache notes

- `px env python` prints the detected interpreter, honoring
  `PX_RUNTIME_PYTHON` before falling back to `python3`/`python` in `$PATH`.
- `px env info` and `px env paths` reuse the same JSON envelope (and `--json`
  switch) so automation can read `interpreter`, `project_root`, `pythonpath`,
  and derived env vars (`PX_PROJECT_ROOT`, `PYTHONPATH`). Human-readable output
  mirrors those fields line-for-line for quick inspection.
- Cache-path policy (Phase A): `PX_CACHE_PATH` overrides everything. Otherwise,
  Unix resolves `XDG_CACHE_HOME` (or `~/.cache`) and Windows resolves
  `LOCALAPPDATA` (falling back to `%USERPROFILE%\AppData\Local`). We always
  append `px/store`, create the directory lazily, and surface the final path via
  `px cache path` (message + JSON `details.path`) along with the source that was
  chosen (`PX_CACHE_PATH`, `XDG_CACHE_HOME`, etc.).

### Lock diff & cache maintenance

- Phase B introduces `px lock diff`, a read-only comparator that reuses the
  drift detector to enumerate dependency adds/removes/changes plus schema
  mismatches (lock version, python requirement, metadata mode). Human output is a
  short status line, while JSON surfaces `{added, removed, changed,
  python_mismatch, version_mismatch, mode_mismatch}` for CI alerts.
- `px cache stats` and `px cache prune` operate on the same resolved cache path
  as `px cache path`. Stats emit file count + total bytes (human + JSON). Prune
  currently supports `--all` (required) and `--dry-run`; it deletes every entry
  beneath the cache root in deterministic order and reports how much space was
  reclaimed (or would be reclaimed when run in dry-run mode).

Each command flows `px-cli → px-core → domain crate`, keeping the CLI thin so
future workspace/multi-target additions plug in without major UX rewrites.

### Lockfile & Install (Phase A slice)

- `px sync` now implements a **pinned-only** path. It requires each
  `[project].dependencies` entry to be an exact `name==version` pin. For every
  pin, px fetches `https://pypi.org/pypi/{name}/{version}/json`, selects the
  best wheel (preferring `py3-none-any`, otherwise the interpreter’s ABI/plat
  tags), downloads it into the px cache, and verifies the SHA256 digest.
- The resulting `px.lock` v1 layout is deterministic:
  - `version = 1`
  - `[metadata]` (`px_version`, `created_at`, `mode = "p0-pinned"`)
  - `[project]` name + `[python].requirement`
  - `[[dependencies]]` tables containing `name`, `specifier`, and
    `artifact.{filename,url,sha256,size,cached_path,python_tag,abi_tag,platform_tag}`
- `px sync --frozen` skips network work and ensures both the manifest and the
  cached artifacts match the lock (missing wheels, size mismatches, or checksum
  drift all trigger a `user-error`).
- `px tidy` reuses the drift detector but never rewrites files; it reports
  whether `px.lock` matches `pyproject` and whether the lock schema is current.
- Future phases will layer multi-target entries (platform/ABI matrices) onto
  this per-dependency artifact schema without breaking existing consumers.

### Workspace orchestration (Phase C slice)

- Workspaces are declared via `[tool.px.workspace]` (see `docs/spec.md:116-125`).
  Phase C reads `members = ["member_alpha", "apps/api"]`, normalizes the paths,
  and records whether each member’s manifest/lock exists.
- `px workspace list` simply echoes those members (human + JSON) so automation
  can visualize the layout. The JSON envelope mirrors the rest of the CLI
  (`docs/cli.md:37`), adding `details.workspace.members[*].{name,path,manifest,
  lock_exists}` for downstream tooling.
- `px workspace verify` reuses the lock/manifest drift logic per member: missing
  manifests, mismatched locks, or dependency drift cause a `user-error` exit and
  fill `details.workspace.members[*].drift`. When every member’s `px.lock`
  matches its manifest the command exits cleanly, giving the workspace a cheap
  sanity check before multi-target locks land.

## Future Growth Path

- **Phase B (Team & CI):** add lock diffing/JSON, IDE shims, offline cache
  flows, Windows parity, and CI-friendly exit codes on top of this structure.
- **Phase C (Workspaces & Multi-Target):** upgrade `px-project` +
  `px-resolver` to understand `[tool.px.workspace]`, evolve `px-lockfile` to
  multi-target entries, and let `px-store` manage per-target wheel caches.
- **Phase D/E (Org & GA):** layer interpreter management, policy/audit hooks,
  transactional installs, `px doctor`, and signed lockfiles without breaking the
  Phase A contracts described above.

This architecture keeps Phase A practical—focused on CAS + deterministic locks +
the bootstrap runner—while leaving clear extension seams for subsequent phases.
