# Goals & Status

## Product-level milestones

- [x] Deterministic CLI surface wired to `px-core`, covering init/add/remove/sync/update/run/test plus supporting verbs such as `status`, `fmt`, `lint`, `build`, and `migrate` (`crates/px-cli/src/main.rs`).
- [x] Resolver + lockfile pipeline that understands pinned dependency graphs and artifacts via `pep440_rs`/`pep508_rs` (`crates/px-resolver`, `crates/px-lockfile`).
- [x] Pyproject authoring that edits `[project]` metadata and dependency arrays directly (PEP 621 aligned) (`crates/px-project/src/manifest.rs`).
- [x] Color-coded Python tracebacks with px-specific remediation hints surfaced under failures in `px-cli`.

## Known placeholders

- [ ] `px fmt` / `px lint` are stubbed in `crates/px-core/src/lib.rs` (lines 613–625); they only echo arguments in the JSON details and never invoke formatters/linters.
- [ ] `px update` is stubbed in `crates/px-core/src/lib.rs` (lines 2773–2781); it emits `"stubbed project update"` without touching the resolver or lockfile.

## PEP compatibility tracker

- [x] **PEP 440** – Version specifiers enforced everywhere via `pep440_rs`.
- [x] **PEP 508** – Requirement parsing, normalization, and marker evaluation handled with `pep508_rs`.
- [ ] **PEP 685** – Resolver still lacks conflict messaging for incompatible direct URLs/extras; no dedicated handling detected.
- [ ] **PEP 241** – Legacy metadata 1.0 not yet emitted or consumed anywhere in the build path.
- [ ] **PEP 314** – Same as above for metadata 1.1.
- [ ] **PEP 345** – Same as above for metadata 1.2.
- [ ] **PEP 566** – Metadata 2.1 support still absent from builders/publishers.
- [ ] **PEP 643** – Dynamic metadata (including SPDX license expression) not surfaced in current manifests.
- [ ] **PEP 639** – License-expression enforcement pending; manifests currently treat `license` opaquely.
- [ ] **PEP 517** – Build backend isolation flow not wired; `px build` command still shells out through legacy paths.
- [ ] **PEP 518** – `build-system` requirements from `pyproject.toml` are not resolved/satisfied before builds.
- [x] **PEP 621** – `[project]` tables are created and edited directly by `px-project`.
- [ ] **PEP 660** – Editable installs are not exposed; resolver rejects URL requirements entirely.
- [ ] **PEP 735** – Optional-dependency group metadata not yet reconciled with lockfile graph.
- [ ] **PEP 376** – Installed-record (`RECORD`) management for px-managed environments not implemented.
- [ ] **PEP 610** – No generation of `direct_url.json` files for direct URL installs.
- [ ] **PEP 427** – Wheel build guarantees not yet enforced when running `px build`.
- [ ] **PEP 425** – Compatibility tag evaluation is stubbed (ResolverTags only track python/abi/platform strings).
- [ ] **PEP 513** – manylinux1 compatibility/tag constraints still missing.
- [ ] **PEP 571** – manylinux2010 untreated.
- [ ] **PEP 599** – manylinux2014 untreated.
- [ ] **PEP 600** – Future manylinux policy versioning untreated.
- [ ] **PEP 656** – musllinux tags untreated.
- [ ] **PEP 503** – Simple repository API consumption not wired; resolver currently uses only the JSON API.
- [ ] **PEP 629** – Normalized simple API responses (including new metadata sections) not yet consumed.
- [ ] **PEP 658** – Embedded metadata (via `data-dist-info` in simple API) unsupported.
- [ ] **PEP 714** – Signature/metadata fetch requirements for `simple` responses unimplemented.
- [ ] **PEP 691** – New JSON-simple API not yet adopted (current resolver still hits `/pypi/<name>/json`).
- [ ] **PEP 592** – Yanked release flags not honored when picking artifacts.
- [ ] **PEP 700** – Source distribution metadata from `sdist` archives not ingested.
- [ ] **PEP 708** – Supply-chain `requires-dist` provenance for lockfiles not implemented.
- [ ] **PEP 740** – Metadata-only wheels/sdists not recognized.
- [ ] **PEP 792** – No support for trusted publisher (PyPI OIDC) metadata flows yet.
- [ ] **PEP 405** – Intentionally not targeted; px uses its own deterministic `.px/envs` manager instead of stdlib `venv`.
- [ ] **PEP 668** – No detection/emission of `EXTERNALLY-MANAGED` markers inside px-managed envs.
- [ ] *(Optional)* **PEP 694** – Upload API work unstarted.
- [ ] *(Optional)* **PEP 807** – Warehouse upload V2 integration unstarted.

## Traceback UX & px recommendations

Every failure that surfaces in `px` eventually reduces to a Python traceback or a CLI level diagnostic. The goal of this section is to ensure both flows share the same opinionated structure: make the failing frame obvious, highlight why px cares, and present the single best next command to copy–paste. The resolver work and lockfile pipeline already make failures deterministic; this UX spec makes those failures actionable.

### Baseline rendering

1. **Color surfacing.** Use the px CLI palette: the `Traceback (most recent call last)` header uses the accent color, file/module names are printed in the primary color, and the error type/message adopt the error color. Everything else (line numbers, code excerpts) stays dim so the eye lands on the important tokens.
2. **Frame formatting.** Each frame keeps the default two-line form but adds inline emphasis—`File "foo.py" line 42 in bar` becomes a single line with the file path underlined and the callable name bolded. The following source line is syntax highlighted with gray gutter characters. Multiline frames (async, chained exceptions) repeat the pattern.
3. **px hint block.** The final line after `ValueError: ...` is reserved for a px recommendation. The label (`Hint`, `Next`, or `px`) stays constant, and the block gets bordered (e.g., `px ▸ Hint: run ...`). If multiple hints qualify, show the highest priority and note that additional context is available via `px explain <code>`.
4. **Copyable commands.** Example commands are always wrapped in backticks, contain no shell prompts, and are already resolved to the active project (no `...` placeholders unless the user must replace text like `<pkg>`).
5. **Structured JSON.** Under `--json` the same metadata is emitted as `{ "traceback": [...], "error": {...}, "recommendation": {...} }` so that editor integrations can show the same hints.

### Recommendation catalog

Each heuristic below maps a concrete failure pattern to a px-branded suggestion. When multiple heuristics trigger, order them by confidence (A → lower letter signals fallback). All detections rely on data the CLI already owns—parsed tracebacks, resolver metadata, pyproject state, and environment fingerprints.

**Missing import (`ModuleNotFoundError`, `ImportError`)**

- *Signal.* Error message `No module named '<mod>'` with `<mod>` not provided by stdlib or installed artifacts in `.px/envs/...`.
- *Copy.* `px ▸ Hint: add '<mod>' with "px add <mod>" and re-run.`
- *Notes.* Use requirement normalization to map packages like `import yaml` → `pyyaml` suggestions when known aliases exist.

**Declared dependency not installed**

- *Signal.* `DistributionNotFound` or resolver metadata indicates dependency listed in `[project.dependencies]` but missing from env hash.
- *Copy.* `px ▸ Hint: run "px install" to sync the environment with px.lock.`

**Environment out of sync with lock**

- *Signal.* Lock hash mismatch (`px.lock` hash differs from `.px/state.json`) even before Python execution.
- *Copy.* `px ▸ Hint: "px install" will recreate the env from px.lock.`

**No px project found**

- *Signal.* `px` invoked outside a directory containing `pyproject.toml` or `.px/` metadata.
- *Copy.* `px ▸ Hint: start a new project with "px init".`

**Legacy project detected**

- *Signal.* `requirements.txt`, `Pipfile`, or non-deterministic env artifacts present without px metadata.
- *Copy.* `px ▸ Hint: convert this project with "px migrate".`

**Dependency version incompatible with Python**

- *Signal.* Resolver failure referencing `Requires-Python` or pyproject marker mismatch against current interpreter.
- *Copy.* `px ▸ Hint: "px env python <version>" selects a compatible interpreter, or pick a release that supports Python ${current}.`

**Dependency version incompatible with constraints**

- *Signal.* Resolver conflict such as `Found existing <pkg>==1.0 but >=2.0 required`.
- *Copy.* `px ▸ Hint: relax the spec in pyproject (e.g. "<pkg>=^2") or upgrade dependents.`

**Wrong interpreter detected**

- *Signal.* Traceback shows `sys.prefix` outside `.px/envs`, indicating user ran `python` directly.
- *Copy.* `px ▸ Hint: use "px run <cmd>" to ensure the px-managed interpreter is active.`

**Missing dev tool**

- *Signal.* CLI invocation fails because `ruff`, `pytest`, etc., are missing while running a px verb that expects them.
- *Copy.* `px ▸ Hint: add the tool with "px add --dev <tool>".`

**Missing build backend**

- *Signal.* `pyproject` lacks `[build-system]` or backend distribution missing.
- *Copy.* `px ▸ Hint: declare a backend such as "hatchling" in [build-system] or add it as a dependency.`

**Command typo**

- *Signal.* Unknown px verb; Levenshtein distance < 3 to a supported verb.
- *Copy.* `px ▸ Hint: did you mean "px <closest>"?` (auto-suggest within the same block).

**ABI mismatch**

- *Signal.* Traceback contains `ImportError: cannot import name ... from partially initialized extension` or `mach-o`/`ELF` mismatch referencing numpy/scipy.
- *Copy.* `px ▸ Hint: reinstall the package or upgrade the interpreter ("px env python <new>" then "px install").`

**Newer versions available**

- *Signal.* Resolver warns about yanked/vulnerable releases or known CVE metadata.
- *Copy.* `px ▸ Hint: update with "px update" to pull in the latest compatible versions.`

**Running Python files outside a project**

- *Signal.* User executes `px run python file.py` without a project, or `python file.py` with px shim.
- *Copy.* `px ▸ Hint: create a project ("px init") or run ad-hoc code via "px run --no-project python file.py".`

**Invalid package spec**

- *Signal.* Resolver/`px add` rejects requirement strings (bad extras, invalid version pins).
- *Copy.* `px ▸ Hint: fix the spec (e.g., "pkg>=1.0") and rerun "px add".`

**Missing script/run target**

- *Signal.* `px run foo` references an undefined script.
- *Copy.* `px ▸ Hint: define "foo" in [tool.px.scripts] or run one of: <autogenerated list>.`

**Missing or unsupported Python version**

- *Signal.* Requested interpreter is not installed locally or not downloadable for platform.
- *Copy.* `px ▸ Hint: install an available version with "px env python <valid>".`

**requirements.txt present**

- *Signal.* Migration guard finds `requirements.txt` without px metadata while running px CLI commands.
- *Copy.* `px ▸ Hint: migrate deterministic dependencies with "px migrate".`

**Multiple conflicting dependency sources**

- *Signal.* Project includes `[project.dependencies]`, `requirements.txt`, and `poetry.lock` simultaneously.
- *Copy.* `px ▸ Hint: pick one source or call "px migrate --from <source>" to unify.`

**`px install` with no dependencies**

- *Signal.* `pyproject` contains zero dependencies and user runs `px install`.
- *Copy.* `px ▸ Hint: add packages with "px add <name>"; install will do nothing until dependencies exist.`

These behaviors give px a single recognizable “panic kit”: consistent colors, deterministic hints, and actionable commands.
