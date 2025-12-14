# Concepts Overview

px is the **front door for Python**. It owns **projects**, **tools**, and **runtimes**, and decides which Python runs, which packages are on `sys.path`, and where they come from. Once px is in charge you should never have to ask “Which Python is this?” or “Why is this dependency even here?”

## Core nouns

* **Project** – a directory with `pyproject.toml` and/or `px.lock` that px manages end-to-end.
* **Workspace** – a set of related px projects in one tree that share a single dependency universe (one lock, one env) for development.
* **Tool** – a named Python CLI installed into its own isolated, CAS-backed environment, runnable from anywhere.
* **Runtime** – a Python interpreter (e.g. 3.10, 3.11) that px knows about and can assign to projects, tools, and workspaces.
* **Sandbox** – optional containerized layer derived from `[tool.px.sandbox]` + an env profile; adds curated system capabilities on top of px envs for `px run --sandbox`, `px test --sandbox`, `px pack image`, and `px pack app`.

Everything else—env directories, caches, and CAS internals—is an implementation detail. Lockfiles are user-facing artifacts, but you shouldn’t need to read them; use `px status`, `px explain`, and `px why` instead.
Inline `px`-annotated scripts are treated internally as tiny, cached px projects backed by the CAS; you don't need to think about them as a new noun.

`px explain` is **execution introspection**: it prints what px *would* execute (runtime selection, profile/source, engine path like `cas_native` vs `materialized_env`, argv/workdir/sys.path) without doing repairs or running the target. Use it when you suspect “wrong Python”, “wrong entrypoint”, or an unexpected CAS-native fallback.

## Core loops

### Project lifecycle

1. `px init` – declare this directory as a px project.
2. `px add ...` – declare dependencies.
3. `px sync` – resolve, lock, and build the environment.
4. `px run ...` / `px test` / `px fmt` – execute in deterministic envs.
5. `px run --sandbox ...` / `px test --sandbox` (optional) – execute inside a reproducible sandbox image when system libraries or container parity are required.
6. Commit `pyproject.toml` and `px.lock`.

### Tool lifecycle

1. `px tool install black`
2. `px tool run black --check .`

Tools are isolated from projects and from each other. Upgrading Python or changing project deps must not silently break them.

### Workspace lifecycle

1. `px init` in member projects.
2. Configure a workspace at the repo root listing member projects.
3. `px sync` – resolve across all members, write a workspace lock, and build a shared env.
4. `px run` / `px test` inside any member – execute in that shared workspace env.
5. Commit the workspace manifest + workspace lock, plus each member’s `pyproject.toml`.

## Design principles (at a glance)

* **Two parallel state machines, same shape** – projects and workspaces each have Manifest/Lock/Env artifacts and state machines; commands are transitions over these machines.
* **Content-addressable store** – artifacts and env materialization rely on a digest-keyed store; see [Content-Addressable Store](../design/content-addressable-store.md) for layout and invariants.
* **Determinism** – given the same inputs, px chooses the same runtimes, lockfiles, envs, and command resolution paths.
* **Sandbox as a derived layer** – sandbox images are derived from env profiles plus a small manifest; they never mutate project/workspace state and stay optional for workflows that need system packages or deployable images.
* **Smooth UX, explicit mutation** – mutating operations are explicit (`init`, `add`, `remove`, `sync`, `update`, `tool install/upgrade/remove`); reader commands never change manifests or lockfiles.

See [Determinism and CI](../design/determinism-and-ci.md) for deeper rationale and CI rules, and [Non-goals](../design/non-goals.md) for boundaries.
