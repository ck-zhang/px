# Env Vars and Flags

Rule of thumb: user-facing behavior should be a CLI flag first; env vars stay as overrides/advanced knobs.

## Global CLI flags (all commands)

* `-q/--quiet` – suppress human-oriented output (errors still print).
* `-v/--verbose` (repeatable) – increase logging; `-vv` reaches trace.
* `--trace` – force trace logging even without `-v`.
* `--json` – emit `{status,message,details}` JSON envelopes.
* `--no-color` – disable colored output.
* `--config <path>` – explicit px config file.
* `--offline` / `--online` – force network mode for this run (sets `PX_ONLINE`).
* `--no-resolver` / `--resolver` – disable/enable dependency resolution (sets `PX_RESOLVER`).
* `--force-sdist` / `--prefer-wheels` – pick sdists vs wheels when both exist (sets `PX_FORCE_SDIST`).

## Shared command flags to remember

* `--dry-run` – preview changes without writing files or building envs (init/add/remove/update/sync/build/publish).
* `--frozen` – fail instead of repairing drift (sync/run/test/fmt); `CI=1` has the same “frozen” effect for run/test.
* `--force` – currently only meaningful for `px init`; bypasses the dirty-worktree guard when scaffolding a project.
* `--interactive` / `--non-interactive` – force stdio mode for `px run`; otherwise px chooses based on the target.

## Command-specific switches (quick scan)

* `px init` – `--package NAME`, `--py VERSION`, `--dry-run`, `--force`.
* `px add/remove/update` – `--dry-run`.
* `px sync` – `--dry-run`, `--frozen`.
* `px run` – `--target NAME`, `--interactive`, `--non-interactive`, `--frozen`, `--` forwards args.
* `px test` – `--frozen`, `--` forwards pytest args.
* `px fmt` – `--frozen`, `--json` (fmt-only), `--` forwards tool args.
* `px status` – `--brief`.
* `px build` – format selector `sdist|wheel|both` (positional), `--out DIR`, `--dry-run`.
* `px publish` – `--registry NAME`, `--token-env VAR` (defaults to `PX_PUBLISH_TOKEN`), `--dry-run` (default), `--upload` (uploads require `PX_ONLINE=1`).
* `px migrate` – `--python VERSION`, `--apply/--write`, `--yes`, `--no-input`, `--source PATH`, `--dev-source PATH`, `--allow-dirty`, `--lock-only`, `--no-autopin`.
* `px why` – `--issue ID` (mutually exclusive with package arg).
* `px tool install` – `--python VERSION`, `--module MODULE`.
* `px tool run` – `--console SCRIPT`, `--` forwards tool args.
* `px tool upgrade` – `--python VERSION`.
* `px python install` – `--path /path/to/python`, `--default`.

## Environment variables

Prefer the flags above for interactive use; env vars remain for CI/automation or process-wide defaults.

### Network, resolution, and downloads

* `PX_ONLINE` – defaults to online; values `0/false/no/off/""` disable network access. Some operations (`px publish`, `px migrate --apply`) require `PX_ONLINE=1`.
* `PX_RESOLVER` – set `0` to skip resolver-driven pin refresh; `1` (default) keeps resolving.
* `PX_FORCE_SDIST=1` – prefer building from sdists even when wheels exist.
* `PX_INDEX_URL` (or `PIP_INDEX_URL`/`PIP_EXTRA_INDEX_URL`) – override package index URLs used for resolution.
* `PX_DOWNLOADS` – max concurrent artifact downloads (clamped 1–16; defaults to available CPUs).
* `PX_PROGRESS=0` – disable spinners/progress lines even on TTYs.

### Runtimes and paths

* `PX_RUNTIME_PYTHON` – explicit interpreter px should use for resolution/env builds (else px finds `python3`/`python`).
* `PX_RUNTIME_REGISTRY` – override the location of the runtime registry file (default `~/.px/runtimes.json`).
* `PX_PYTHON_DOWNLOADS_URL` – alternate Python downloads manifest for `px python install` (supports `http(s)://` or `file://`).
* `PX_CACHE_PATH` – root for the shared artifact cache (default under `$HOME/.cache/px/store`).
* `PX_TOOLS_DIR` – root for installed tools (metadata/locks/envs; default `~/.px/tools`).
* `PX_TOOL_STORE` – location for cached tool environments (default `~/.px/tools/store/envs`).
* `PX_NO_ENSUREPIP=1` – skip the automatic `python -m ensurepip --default-pip --upgrade` when px refreshes a project site. By default px runs ensurepip only when pip is absent in the project’s `.px` site, and it also seeds a baseline `setuptools` so `python setup.py`/`pkg_resources` keep working without declaring it.
* `PX_DEBUG_SITE_PATHS=/path/to/file` – write the final `sys.path` computed by px’s `sitecustomize` to the given file (for debugging path issues).

### Dependency selection and execution behavior

* `PX_GROUPS` – add extra dependency groups at runtime; comma/semicolon/whitespace separated, PEP 503-normalized.
* `CI=1` – treat `px run`/`px test` as frozen (no auto-repair of env/lock drift).
* `PX_TOOL_PASSTHROUGH=1` – force `px tool run` to attach stdio even when forwarding args (default passthrough only when no args).
* `PX_TEST_REPORTER=pytest` – use pytest’s native reporter for `px test` instead of px’s default summary.
* `PX_PUBLISH_TOKEN` – default token env var consumed by `px publish` (override name via `--token-env`).
