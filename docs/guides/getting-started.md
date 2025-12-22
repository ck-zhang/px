# Getting Started

px is the front door for Python projects: it owns the runtime, dependencies, and environment layout so commands are deterministic.

px supports Linux, macOS, and Windows.

## Quick start (single project)

If `px init` reports `python runtime unavailable`, register a runtime first (e.g. `px python install 3.12`, or `px python install 3.12 --path /path/to/python3.12`).

1. From your project root, run `px init` to create `pyproject.toml`, `px.lock`, and `.px/`.
2. Declare dependencies with `px add <pkg>...`.
3. Materialize or refresh the env with `px sync` (auto-resolves if needed).
4. Run code or tests with `px run <target>` or `px test`; formatting via `px fmt`.
5. Commit `pyproject.toml`, `px.lock`, and source; `.px/` stays untracked.

## What px manages

* **Runtime** – a Python interpreter px knows about; chosen deterministically from `[tool.px].python`, `[project].requires-python`, or px default.
* **Manifest** – dependencies in `pyproject.toml`.
* **Lockfile** – `px.lock`, generated only by px; don’t edit by hand.
* **Env** – project-local pointer at `.px/envs/current` to a global env materialization under `~/.px/envs/<profile_oid>` (tied to the lock and runtime).

## When things drift

* Manifest changed but lock didn’t? `px sync` (fails under `--frozen`/CI).
* Env missing/outdated? `px sync` rebuilds it (unless `--frozen`).
* Missing import in `px run`? Add it (`px add <pkg>`) or resync if already declared.

## Running scripts

px understands inline metadata blocks at the top of a script (PEP 723 style):

```
# /// script
# requires-python = ">=3.11"
# dependencies = ["httpx"]
# ///
```

You can also pin more than one dependency:

```
# /// script
# requires-python = ">=3.10"
# dependencies = ["rich==13.9.2", "requests<3"]
# ///
```

Run the file with `px run script.py`; px resolves the inline deps, builds an isolated env in its cache, and reuses it next time.

You can also run a script from a repository reference (no local project required):

* `px run gh:ORG/REPO@<sha>:path/to/script.py`
* `px run git+file:///abs/path/to/repo@<sha>:path/to/script.py` (offline-testable)

These forms are pinned-by-default; floating refs require `--allow-floating` and are refused under `--frozen` or `CI=1`. In `--offline` mode, the repo snapshot must already be cached in the CAS.

## Trying px without adopting

If you want to run in a non-px directory (no `.px/` or `px.lock` writes), use `--ephemeral` (alias `--try`):

* `px run --ephemeral <target> [...args]`
* `px test --ephemeral`

px reads dependency inputs from the current directory (`pyproject.toml` or `requirements.txt`; scripts with a PEP 723 block use that instead) and stores all state in the global cache. To adopt the directory and commit a real `px.lock`, run `px migrate --apply`.

## Going further

- Multi-project repo? See [Workspaces](./workspaces.md).
- Global tools? See [Tools](./tools.md).
- Python installations? See [Runtimes](./runtimes.md).
