# Getting Started

px is the front door for Python projects: it owns the runtime, dependencies, and environment layout so commands are deterministic.

px currently supports Linux and macOS only; Windows is not supported yet. Use WSL or a Unix host for now.

## Quick start (single project)

1. From your project root, run `px init` to create `pyproject.toml`, `px.lock`, and `.px/`.
2. Declare dependencies with `px add <pkg>...`.
3. Materialize or refresh the env with `px sync` (auto-resolves if needed).
4. Run code or tests with `px run <target>` or `px test`; formatting via `px fmt`.
5. Commit `pyproject.toml`, `px.lock`, and source; `.px/` stays untracked.

## What px manages

* **Runtime** – a Python interpreter px knows about; chosen deterministically from `[tool.px].python`, `[project].requires-python`, or px default.
* **Manifest** – dependencies in `pyproject.toml`.
* **Lockfile** – `px.lock`, generated only by px; don’t edit by hand.
* **Env** – under `.px/envs/...`, tied to the lock and runtime.

## When things drift

* Manifest changed but lock didn’t? `px sync` (fails under `--frozen`/CI).
* Env missing/outdated? `px sync` rebuilds it (unless `--frozen`).
* Missing import in `px run`? Add it (`px add <pkg>`) or resync if already declared.

## Running scripts

px understands inline metadata blocks at the top of a script (PEP 723 style):

```
# /// px
# requires-python = ">=3.11"
# dependencies = ["httpx"]
# ///
```

You can also pin more than one dependency:

```
# /// px
# requires-python = ">=3.10"
# dependencies = ["rich==13.9.2", "requests<3"]
# ///
```

Run the file with `px run script.py`; px resolves the inline deps, builds an isolated env in its cache, and reuses it next time.

## Going further

* Multi-project repo? See `docs/guides/workspaces.md`.
* Global tools? See `docs/guides/tools.md`.
* Python installations? See `docs/guides/runtimes.md`.
