# Errors and Troubleshooting

## Error shape

All user-facing errors follow:

```text
PX123  <short summary>

Why:
  • <one or more bullet points>

Fix:
  • <one or more bullet points with copy-pasteable commands>
```

Color: code + summary in error color; “Why” bullets normal; “Fix” bullets accent. Python tracebacks show after the px error summary by default; full raw trace only under `--debug`.

## Common heuristics

* **No project found** – suggest `px init`.
* **Lock missing / out-of-sync** – suggest `px sync` (fail under `--frozen`).
* **Missing import in `px run`** – suggest `px add <pkg>` or `px sync` depending on whether `<pkg>` is already in M/L.
* **Wrong interpreter (user ran `python` directly)** – suggest `px run python ...`.
* **Suspect wrong Python / wrong entrypoint / unexpected engine path** – use `px explain run ...` (and `-v/-vv` for fallback codes) or `px explain entrypoint <name>` to see what px would execute without running anything.
* **Ambiguous console script name** – px normally resolves this by falling back to a materialized env (so the `bin/` winner is deterministic). If you still see `ambiguous_console_script`, remove one of the conflicting dependencies, or run a specific module via `px run python -m <module>`.
* **Runtime mismatch for tool** – suggest `px tool install <tool>` again or `px python install`.
* **Mutating pip under `px run`** – **Why**: px envs are immutable CAS materializations; `pip install/uninstall` cannot change them. **Fix**: use `px add/remove/update/sync` to update dependencies, then rerun the command with `px run`.
* **CAS-native fallback happened** – px may automatically fall back to a materialized env when CAS-native execution hits packaging/runtime quirks. Re-run with `-v` / `-vv` and look for a single log line containing `CAS_NATIVE_FALLBACK=<code>`, or inspect `--json` output under `details.cas_native_fallback`.

## Flags and CI behavior

* `-q / --quiet` – only essential output.
* `-v, -vv` – progressively more detail.
* `--debug` – full logs, internal details, stack traces.
* `--json` – structured output where applicable.

Under `CI=1` or explicit `--frozen`:

* No prompts.
* No auto-resolution.
* `px run` / `px test` / `px fmt` do not rebuild project/workspace envs; they just check consistency and fail if broken (for `run`/`test`) or run tools in isolation (`fmt`).

## Non-TTY and structured output

If stderr is not a TTY or `--json` is set:

* No spinners, progress bars, or frame-based animations.
* Progress is line-oriented logs or structured events inside `--json`.
* Repeated progress updates should be throttled/collapsed; output ordering must be stable for a given command and state.

Applies to all commands that show progress (resolver, env build, tool install, etc.).

## Troubleshooting (error codes → required transitions)

* `missing_lock` (`PX120`): run `px sync` (without `--frozen`) to create or refresh `px.lock`.
* `lock_drift` (`PX120`): run `px sync` to realign `px.lock` with the manifest/runtime; frozen commands must refuse.
* `missing_env` / `env_outdated` (`PX201`): run `px sync` to (re)build the relevant project/workspace env; `--frozen` refuses to repair.
* `runtime_mismatch`: run `px sync` after activating the desired Python, or pin `[tool.px].python`.
* `invalid_state`: delete or repair `.px/state.json` and retry; state is validated and rewritten atomically.
* `pyc_cache_unwritable`: px could not create the Python bytecode cache directory; ensure `~/.px/cache` (or `PX_CACHE_PATH`) is writable and retry. If bytecode caches grow too large, it is always safe to delete `~/.px/cache/pyc`.
* `ambiguous_console_script`: multiple dists provide the same `console_scripts` name; px typically falls back to a materialized env to pick a deterministic winner, but if fallback is unavailable, remove one of the conflicting deps (or run a specific module via `px run python -m <module>`).

## Run by reference (`gh:` / `git+`)

Run-by-reference targets fetch/cache a commit-pinned repo snapshot in the CAS and execute a Python script from it.

* `run_reference_requires_pin`: the repo ref is floating (branch/tag/no `@`), but pinned commits are required by default.

  * Fix: use `@<full_sha>` (recommended), e.g. `px run gh:ORG/REPO@<sha>:path/to/script.py`
  * Fix: or allow floating refs explicitly: `px run --allow-floating <TARGET> …` (refused under `--frozen` or `CI=1`)

* `run_reference_requires_full_sha`: the ref after `@` looks like a short commit SHA (or otherwise isn’t a full pinned SHA).

  * Fix: use a full 40‑character commit SHA (recommended), e.g. `px run gh:ORG/REPO@<full_sha>:path/to/script.py`
  * Fix: or resolve it explicitly: `px run --allow-floating <TARGET> …` (refused under `--frozen` or `CI=1`)

* `run_reference_offline_missing_snapshot`: `--offline` / `PX_ONLINE=0` was set, but the snapshot is not cached yet.

  * Fix: re-run once without `--offline` to populate the CAS, then retry with `--offline`

* `run_reference_offline_floating`: floating refs require online mode (even if the repo is local).

  * Fix: pin a full commit SHA, or re-run with `--online` / `PX_ONLINE=1`

* `run_reference_floating_disallowed`: floating refs were requested under `--frozen` or `CI=1`.

  * Fix: pin a full commit SHA and retry

* `invalid_run_reference_target`: the target is malformed.

  * Fix: use `gh:ORG/REPO@<sha>:path/to/script.py` or `git+file:///abs/path/to/repo@<sha>:path/to/script.py`

* `invalid_repo_snapshot_locator` / `invalid_run_reference_locator`: the git locator is invalid (or contains credentials/query/fragment).

  * Fix: use a plain locator like `git+https://host/org/repo.git` or `git+file:///abs/path/to/repo` (no embedded credentials; use a git credential helper instead)

## Sandbox errors

Sandbox errors are prefixed `PX9xx` and do not change manifests/locks/envs.

* `PX900` (sandbox base unavailable) – base name is unknown/incompatible; change `[tool.px.sandbox].base` or upgrade px.
* `PX901` (capability resolution failure) – capability cannot be satisfied on the chosen base; pick another base or disable that capability.
* `PX902` (missing system dependency) – sandbox image lacks a required library (e.g., `libpq.so.5`); add the capability (e.g. set `[tool.px.sandbox.capabilities].postgres = true`) and rerun.
* `PX903` (sandbox build failure) – underlying image build/backend failed; check disk space/registry credentials and retry.
* `PX904` (sandbox format/version mismatch) – sandbox image was built with an incompatible `sbx_version` or px version; rebuild with the current px or clear the sandbox store.
