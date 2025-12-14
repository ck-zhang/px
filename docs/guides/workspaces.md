# Workspaces

A workspace lets multiple px projects in one tree share a single dependency universe (one lock, one env) for development.

## When to use

* Multi-project repos where members should share deps and runtime.
* You want a single lock/env to guarantee consistency across services/packages.

## Setup

1. Ensure each member project has a `pyproject.toml` (for a new repo, running `px init` in each member is the easiest way to scaffold one).
2. At the repo root, add `[tool.px.workspace]` to `pyproject.toml` and list member project paths.
3. Run `px sync` from the workspace root or any member:

   * Resolves across all members.
   * Writes `px.workspace.lock`.
   * Builds a shared workspace env under `.px/` at the workspace root.

4. Commit the workspace manifest and `px.workspace.lock`, plus each member’s `pyproject.toml`.

Notes:

* Once a workspace governs a member, per-project `px.lock` files for members are ignored (px uses `px.workspace.lock` instead). `px sync` won’t update member `px.lock`.
* Workspace state is stored under `.px/` at the workspace root; member-local `.px/` pointers are not used while the workspace governs that member.

## Command routing inside a workspace

* **Discovery** – px walks upward from CWD; if it finds a workspace root and CWD is inside a listed member, that project is workspace-governed.
* **Deps and envs** – member commands use the workspace lock/env (`px.workspace.lock` and workspace env), not per-project `px.lock` or envs.
* **`px add/remove` from a member** – edits that member’s manifest, re-resolves the workspace graph, updates `px.workspace.lock`, rebuilds the workspace env.
* **`px sync`** – refreshes workspace lock/env; never writes per-project `px.lock` for members.
* **`px run` / `px test`** – use the workspace env; in dev may rebuild it from the workspace lock, in CI require it to be already consistent.
* **`px status`** – at workspace root reports workspace state and member inclusion; inside a member reports both workspace state and whether that member manifest was included.

## Workspace states

Workspace artifacts mirror projects:

* **WM** – workspace manifest (`[tool.px.workspace]` and member manifests).
* **WL** – `px.workspace.lock` with a fingerprint of WM.
* **WE** – workspace env tied to WL and runtime.

A workspace is **consistent** when `px.workspace.lock` matches the workspace manifest and a workspace env exists for that lock/runtime. Drift is fixed via `px sync` (dev) or reported as an error under `--frozen`/CI.
