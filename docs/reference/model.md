# Model

px distinguishes projects, workspaces, and the artifacts they own. This document describes how roots are discovered and how manifests, locks, and environments relate to each other.

## Roots and discovery

**Project root** – directory containing either:

* `pyproject.toml` with `[tool.px]`, or
* `px.lock`.

**Workspace root** – directory containing:

* `pyproject.toml` with `[tool.px.workspace]`.

Workspace and project roots may coincide, but they are conceptually separate.

**Project-level command discovery**

1. Starting from CWD, walk upward until you find a workspace root.
2. If found and CWD is inside a listed member project, that project is workspace-governed: project commands use the workspace state machine.
3. Otherwise, walk upward to find a project root (no workspace above).
4. If none is found: `No px project found. Run "px init" in your project directory first.` (or `px migrate --apply` when a non-px `pyproject.toml` already exists).

**Workspace-level discovery**

* To reason about workspaces (`status`, `sync`, etc.), px finds the nearest workspace root above CWD via `[tool.px.workspace]`.

## px-owned artifacts

px may create/modify only:

* **User-facing / shared (per project):**

  * `pyproject.toml`

    * px edits only `[project]` (PEP 621) and `[tool.px]` sections.
    * Sandbox config lives under `[tool.px.sandbox]` (project or workspace root) and is read-only from px’s perspective beyond normal manifest writes.

  * `dist/` – build artifacts (sdist, wheels).

* **px-specific (per project):**

  * `px.lock` – locked dependency graph for this project when it is managed standalone (no governing workspace).
  * `.px/`

    * `.px/envs/` – envs owned by this project or workspace.
    * `.px/logs/` – logs.
    * `.px/state.json` – metadata (current env ID(s), stored lock_id(s), runtime/platform fingerprints; validated and rewritten atomically).

* **px-specific (per workspace root):**

  * `[tool.px.workspace]` in `pyproject.toml` – workspace manifest (WM), including member project paths and optional shared settings.
  * `px.workspace.lock` – workspace lock (WL) describing the union dependency graph of all members.
  * Workspace env metadata under `.px/` at the workspace root (WE). Physical layout is an implementation detail, but workspace envs are distinguishable from per-project envs and are always tied to `px.workspace.lock` and a runtime.

px must not create other top-level files or directories.

**Sandbox artifacts (derived)**

* px may materialize sandbox bases/images under a global sandbox store (e.g., `~/.px/sandbox/`) when `--sandbox` or `px pack image` is used.
* Sandbox artifacts are derived from env profiles plus `[tool.px.sandbox]`; they never add files to project/workspace roots.
* Workspace governs sandbox config when present: workspace root `[tool.px.sandbox]` is authoritative; member-level `[tool.px.sandbox]` is ignored (expect a warning from `px status`).

## Shape after key commands

* After `px init` in an empty dir:

  ```text
  myapp/
    pyproject.toml
    px.lock
    .px/
  ```

* After `px add` / `px sync`:

  Same as above, with `pyproject.toml` dependencies and `px.lock` graph populated, and `.px/envs/...` containing a built env.

* After `px build`:

  ```text
  myapp/
    pyproject.toml
    px.lock
    .px/
    dist/
      myapp-0.1.0.tar.gz
      myapp-0.1.0-py3-none-any.whl
  ```

For a workspace, the root has the workspace manifest/lock/env metadata plus the usual project artifacts if it is also a project.

## Lockfiles

Two lockfile types:

* **Project lockfile** – `px.lock` in a project root:

  * Authoritative description of that project’s environment when the project is not governed by a workspace.
  * Exact versions, hashes, markers, index URLs, platform tags.
  * A fingerprint of `[project].dependencies` (and any px-specific dep config, including `[tool.px].manage-command` and `[tool.px].plugin-imports`).

* **Workspace lockfile** – `px.workspace.lock` in a workspace root:

  * Authoritative description of the shared environment for all member projects.
  * Full resolved dependency graph across all members.
  * Mapping of each package node to its owning project (member) or “external”.
  * A workspace manifest fingerprint of the combined member manifests.

Both lockfiles are machine-generated only; direct edits are unsupported.

## Environments

**Project environment**

* px-managed environment under `.px/envs/...` tied to a project lock (`px.lock`) and a runtime/platform.
* Contains exactly the packages described by that project’s lock.
* Materializes a venv-like layout (`site/lib/pythonX.Y/site-packages`) with `px.pth`/`sitecustomize.py` and python shims under `site/bin` so tools that expect VIRTUAL_ENV-style markers behave consistently.
* Used only when the project is not governed by a workspace. In a workspace, member projects use the workspace env.

**Workspace environment**

* px-managed environment under `.px/envs/...` at the workspace root.
* Tied to `px.workspace.lock` hash, runtime, and platform.
* Contains exactly the packages described by WL.
* Member projects run against WE; per-project envs are ignored in that context.

**Sandbox layer**

* Optional containerized layer on top of a project/workspace env: base OS + resolved capabilities + env profile → sandbox image.
* Configured via `[tool.px.sandbox]` (base, capabilities, inference rules).
* Used by `px run --sandbox`, `px test --sandbox`, and `px pack image`; sandbox images are immutable, cacheable, and do not mutate manifests, locks, or envs.

## Self-consistency

**Project self-consistent** when:

* `pyproject.toml` and its governing lock agree:

  * standalone project → `px.lock` fingerprint matches;
  * workspace-governed project → its manifest is included in the workspace manifest fingerprint, and WL matches that combined fingerprint.
* An environment exists whose identity matches that lock (project env or workspace env).
* `px status` reports `Environment in sync with lock`.

**Workspace self-consistent** when:

* `[tool.px.workspace]` exists and matches `px.workspace.lock` (workspace manifest fingerprint).
* A workspace env exists and matches `px.workspace.lock`.
* `px status` at the workspace root reports the workspace as `Consistent`.

Mutating commands must leave the relevant object (project or workspace) self-consistent on success or fail without partial changes.

## Dependency groups

* Active dependency groups are controlled by `[tool.px.dependencies].include-groups` (PEP 503–normalized names). This list is authoritative for resolution, locking, and env sync.
* If `include-groups` is absent, px enables all declared groups: entries under `[dependency-groups]` and common dev-style optional deps (`dev`, `test`, `doc`, `px-dev`, etc.). `PX_GROUPS` can extend this set at runtime.
* The selected groups are part of the manifest fingerprint and lock drift detection for both projects and workspaces.
* `px migrate --apply` writes `include-groups` covering all declared groups so migrated projects get dev/test/doc dependencies without extra setup.
