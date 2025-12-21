# Env Vars and Flags

Rule of thumb: user-facing behavior should be a CLI flag first; env vars stay as overrides/advanced knobs.

## Global CLI flags (all commands)

* `-q/--quiet` – suppress human-oriented output (errors still print).
* `-v/--verbose` (repeatable) – increase logging; `-vv` reaches trace.
* `--trace` – force trace logging even without `-v`.
* `--debug` – enable debug output and full tracebacks.
* `--json` – emit `{status,message,details}` JSON envelopes (for `px fmt`, `px fmt --json` is an equivalent shortcut). Commands that normally attach stdio will run non-interactively so stdout stays machine-readable.
* `--no-color` – disable colored output.
* `--offline` / `--online` – force network mode for this run (sets `PX_ONLINE`).
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
* `px run` – `--target TARGET`, `--allow-floating`, `--at GIT_REF`, `--interactive`, `--non-interactive`, `--sandbox`, `--frozen` (args after the target are forwarded).
* `px test` – `--sandbox`, `--frozen`, `--at GIT_REF`, `--` forwards test runner args (e.g. pytest flags).
* `px fmt` – `--frozen`, `--json` (JSON output), `--` forwards tool args.
* `px status` – `--brief`.
* `px build` – format selector `sdist|wheel|both` (positional), `--out DIR`, `--dry-run`.
* `px publish` – `--registry NAME`, `--token-env VAR` (defaults to `PX_PUBLISH_TOKEN`), `--dry-run` (default), `--upload` (uploads require `PX_ONLINE=1`).
* `px pack image` – `--tag NAME`, `--out PATH`, `--push`, `--allow-dirty`.
* `px pack app` – `--out PATH`, `--allow-dirty`, `--entrypoint CMD`, `--workdir DIR`.
* `px migrate` – `--python VERSION`, `--apply/--write`, `--yes`, `--no-input`, `--source PATH`, `--dev-source PATH`, `--allow-dirty`, `--lock-only`, `--no-autopin`.
* `px why` – `--issue ID` (mutually exclusive with package arg).
* `px tool install` – `--python VERSION`, `--module MODULE`.
* `px tool run` – `--console SCRIPT` (args after the tool name are forwarded; use `--` if you want to pass flags without ambiguity).
* `px tool upgrade` – `--python VERSION`.
* `px python install` – `--path /path/to/python`, `--default`.
* `px completions` – `bash|zsh|fish|powershell` (positional).

## Environment variables

Prefer the flags above for interactive use; env vars remain for CI/automation or process-wide defaults.

### Network, resolution, and downloads

* `PX_ONLINE` – defaults to online; values `0/false/no/off/""` disable network access. Some operations (`px publish`, `px migrate --apply`) require `PX_ONLINE=1`.
* `PX_FORCE_SDIST=1` – prefer building from sdists even when wheels exist.
* `PX_INDEX_URL` (or `PIP_INDEX_URL`/`PIP_EXTRA_INDEX_URL`) – override package index URLs used for resolution.
* `PX_DOWNLOADS` – max concurrent artifact downloads (clamped 1–16; defaults to available CPUs).
* `PX_PROGRESS=0` – disable spinners/progress lines even on TTYs.

### Runtimes and paths

* `PX_RUNTIME_PYTHON` – explicit interpreter px should use for resolution/env builds. If unset, px selects a registered runtime from the runtime registry (`px python install` / `PX_RUNTIME_REGISTRY`); if no runtime is registered, commands that need Python will fail with a hint to install one.
* `PX_RUNTIME_REGISTRY` – override the location of the runtime registry file (default `~/.px/runtimes.json`).
* `PX_PYTHON_DOWNLOADS_URL` – alternate Python downloads manifest for `px python install` (supports `http(s)://` or `file://`).
* `PX_CACHE_PATH` – root for the shared download/build cache (wheels, sdist builds, Python downloads manifest, etc; default `~/.px/cache`). If set and the other roots below are unset, px will derive them from the same base directory (e.g. `PX_CACHE_PATH=/tmp/px/cache` implies `PX_STORE_PATH=/tmp/px/store`, `PX_ENVS_PATH=/tmp/px/envs`, etc.).
* `PX_STORE_PATH` – root for the content-addressable store (CAS) (default `~/.px/store`).
* `PX_ENVS_PATH` – root for global env materializations (default `~/.px/envs`).
* `PX_SANDBOX_STORE` – root for sandbox images/bases (default `~/.px/sandbox`).
* `PX_TOOLS_DIR` – root for installed tools (metadata/locks; default `~/.px/tools`).
* `PX_TOOL_STORE` – root for cached tool environments (default `~/.px/tools/store`; tool envs live under `<PX_TOOL_STORE>/envs/`).
* `PX_NO_ENSUREPIP=1` – skip the automatic `python -m ensurepip --default-pip --upgrade` when px refreshes an env. `pip`/`setuptools` are treated as implicit base packages (not recorded in `px.lock`); `px why pip` and `px why setuptools` report them as implicit.
* `PX_DEBUG_SITE_PATHS=/path/to/file` – write the final `sys.path` computed by px’s `sitecustomize` to the given file (for debugging path issues).

### Dependency selection and execution behavior

* `PX_GROUPS` – add extra dependency groups at runtime; comma/semicolon/whitespace separated, PEP 503-normalized.
* `CI=1` – treat `px run`/`px test` as frozen (no auto-repair of env/lock drift).
* `PX_TOOL_PASSTHROUGH=1` – force `px tool run` to attach stdio even when forwarding args (default passthrough only when no args).
* `PX_TEST_REPORTER=pytest` – use pytest’s native reporter for `px test` instead of px’s default summary.
* `PX_PUBLISH_TOKEN` – default token env var consumed by `px publish` (override name via `--token-env`).
