# Getting Started

px is the front door for Python projects: it owns the runtime, dependencies, and environment layout so commands are deterministic.

px supports Linux, macOS, and Windows.

## Install (from source)

px is currently built from source:

```sh
cargo install --path crates/px-cli
px --help
```

## Quick start (single project)

px-managed projects require a px-registered Python runtime; `px init` never silently adopts whatever `python` is on PATH.

If no runtime is registered, `px init` prompts to install a default runtime (TTY only). In CI/non-TTY, it fails immediately and prints the exact `px python install <version>` command to run.

1. From your project root, run `px init` to create `pyproject.toml`, `px.lock`, and `.px/`.
2. Declare dependencies with `px add <pkg>...`.
3. Materialize or refresh the env with `px sync` (auto-resolves if needed).
4. Run code or tests with `px run <target>` or `px test`; formatting via `px fmt`.
5. Commit `pyproject.toml`, `px.lock`, and source; `.px/` is machine-local state and should not be committed (`px init` adds `.px/` to `.gitignore` when inside a git repo).

## What px manages

* **Runtime** – a Python interpreter px knows about; chosen deterministically from `[tool.px].python`, `[project].requires-python`, or px default.
  * If multiple runtimes satisfy constraints, px uses the px default runtime.
* **Manifest** – direct dependency intent in `pyproject.toml` (ranges or unpinned). Exact pins live in `px.lock` so you can keep intent and determinism separate.
* **Lockfile** – `px.lock`, generated only by px; don’t edit by hand. It records the full resolved graph (including transitive deps) plus artifact identity (hashes/URLs), and the env is built from it. `px.lock` is portable and commit-safe: it contains no absolute paths, usernames, or local cache layout.
* **Env** – project-local pointer at `.px/envs/current` to a global env materialization under `~/.px/envs/<profile_oid>` (tied to the lock and runtime). `.px/` is machine-local and is not meant for source control.

## Dependency model (manifest + lock)

* `px add` updates `pyproject.toml`, then updates `px.lock` and the env.
* `px update` re-resolves and rewrites `px.lock`, then syncs the env (without churning `pyproject.toml` unless you opt in).

### Opt into manifest pinning

* Per command: `px add --pin <pkg>` writes an exact pin into `pyproject.toml`.
* Per project: set `[tool.px].pin-manifest = true` to pin direct dependencies in `pyproject.toml` by default.

Determinism comes from committing `px.lock` (exact versions + hashes) and using `--frozen`/`CI=1`, not from pinning the manifest.

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
