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
* **Runtime mismatch for tool** – suggest `px tool install <tool>` again or `px python install`.
* **Mutating pip under `px run`** – **Why**: px envs are immutable CAS materializations; `pip install/uninstall` cannot change them. **Fix**: use `px add/remove/update/sync` to update dependencies, then rerun the command with `px run`.

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

## Sandbox errors

Sandbox errors are prefixed `PX9xx` and do not change manifests/locks/envs.

* `PX900` (sandbox base unavailable) – base name is unknown/incompatible; change `[tool.px.sandbox].base` or upgrade px.
* `PX901` (capability resolution failure) – capability cannot be satisfied on the chosen base; pick another base or disable that capability.
* `PX902` (missing system dependency) – sandbox image lacks a required library (e.g., `libpq.so.5`); add the capability (`px sandbox add postgres` or set `[tool.px.sandbox.capabilities].postgres = true`) and rerun.
* `PX903` (sandbox build failure) – underlying image build/backend failed; check disk space/registry credentials and retry.
* `PX904` (sandbox format/version mismatch) – sandbox image was built with an incompatible `sbx_version` or px version; rebuild with the current px or clear the sandbox store.
