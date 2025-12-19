# Tools

px can manage standalone CLI tools in their own locked environments, separate from projects and from each other.

## Why px tools

* Tools stay stable even if project deps change.
* Python upgrades don’t silently break installed tools; the runtime is recorded with each tool.

## Typical workflow

1. Install: `px tool install black` (or any Python CLI). Optionally pin runtime with `--python 3.11`.
2. Run: `px tool run black --check .`
3. Upgrade: `px tool upgrade black`
4. Remove: `px tool remove black`
5. Inspect: `px tool list`

## Behavior notes

* Tools live under `~/.px/tools/...` with their own `tool.lock` and env.
* `px tool run` uses the runtime recorded in `tool.lock`; if it is missing, px fails with a clear error instead of falling back.
* Project state (manifest/lock/env) is unaffected by tool installs and runs.
* `px fmt` uses tool envs only—it never touches project envs or locks. Missing tools are auto-installed into the tool store.
