# px Command Contracts (Pre & Post Conditions)

This document defines, for core px commands:

* **Preconditions** – what must be true before the command runs.
* **Postconditions** – what the command guarantees on success (files, env, invariants).
* **Failure behavior** – what happens when preconditions aren’t met.

It’s about **interactions**, not implementation details.

---

## Shared definitions

* **Project root**
  Directory containing `pyproject.toml` with `[tool.px]`, or `px.lock`.
  Commands start from CWD and walk upward to find it.

* **Project environment**
  px-managed environment located under `.px/envs/...` for this project.

* **Self-consistent project**

  * `pyproject.toml`, `px.lock`, and the project environment all agree on:

    * Python version
    * platform
    * resolved dependency graph
  * `px status` reports: “Environment is in sync with px.lock”.

Unless otherwise noted, **successful commands must leave the project self-consistent**.

---

## `px init`

**Intent**
Initialize a new px project in the current directory.

**Preconditions**

* CWD has no existing px project root above it (otherwise we’re inside an existing project).
* If `pyproject.toml` exists, it is either:

  * empty/minimal, or
  * does not already declare a different packaging tool as the sole authority (poetry-only, etc.).
    (If so, `px init` should refuse and tell the user to run `px migrate` instead.)

**Postconditions (on success)**

* `pyproject.toml` exists with at least:

  * `[project]` with `name`, `version`, `requires-python`, `dependencies = []`.
  * `[tool.px]` stub section.
* `.px/` directory exists with:

  * project environment under `.px/envs/...` for a chosen Python version.
  * internal metadata (`state.json`, logs, etc.).
* `px.lock` exists with:

  * recorded Python version + platform.
  * empty dependency graph.
* Project is **self-consistent** (env matches empty lock).

**Failure behavior**

* If an existing non-px tool is clearly in control (e.g. poetry.lock + no `[tool.px]`), `px init` must refuse with a clear message suggesting `px migrate`.
* On failure, **no new files** are left behind other than optional logs in `.px/`.

---

## `px add <pkg>…`

**Intent**
Add one or more dependencies and make them available immediately.

**Preconditions**

* Project root exists.
* `pyproject.toml` is readable and contains `[project]`.
* If `px.lock` does not exist, it will be treated as if `px init` had just created it (bootstrap lock).

**Postconditions (on success)**

* `pyproject.toml` `[project].dependencies` updated to include the new package(s).
* Dependencies are resolved to a full graph.
* `px.lock` written/updated with:

  * updated dependency graph.
  * updated lock hash.
* Project environment is updated to match the new `px.lock`.
* Project is **self-consistent**:

  * `px status` reports env in sync; new packages are importable in `px run`.

**Guarantee**

* After `px add foo`, `px run ...` can import `foo` without requiring an extra `px sync`.

**Failure behavior**

* On resolution failure: no changes to `pyproject.toml`, `px.lock`, or env.
* Errors must be reported in “What / Why / Fix” structure, with a copy-pasteable suggested command.

---

## `px remove <pkg>…`

**Intent**
Remove one or more dependencies and update everything accordingly.

**Preconditions**

* Same as `px add`.
* The packages to be removed are either:

  * explicitly listed in `[project].dependencies`, or
  * the command clearly reports that they are only transitive (and refuses/reminds the user).

**Postconditions (on success)**

* `pyproject.toml` `[project].dependencies` updated.
* `px.lock` re-resolved and written.
* Project environment updated to match the new `px.lock`.
* Project is **self-consistent**.

**Failure behavior**

* If the named package is not a direct dependency, `px remove` must not silently do nothing. It should:

  * either refuse (“X is not a direct dependency; it’s required by Y”), or
  * provide a `px why <pkg>` hint.
* On failure, no partial updates to `pyproject.toml`, `px.lock`, or env.

---

## `px sync`

**Intent**
Bring the project environment into sync with declared state. Primarily used:

* after a fresh clone, or
* after lockfile-only changes (`px lock upgrade`), or
* in CI.

**Preconditions**

* Project root exists.

**Behavior**

* If `px.lock` *exists*:

  * Install according to `px.lock` into the project environment.
* If `px.lock` *does not exist*:

  * If `[project].dependencies` is empty:

    * No-op (but create an empty `px.lock` if you want).
  * If `[project].dependencies` is non-empty:

    * Resolve, create `px.lock`, and install into project env.

**Postconditions (on success)**

* `px.lock` exists and represents the resolved dependencies.
* Project environment matches `px.lock`.
* Project is **self-consistent**.

**Failure behavior**

* In `--frozen` / CI mode:

  * If `px.lock` is missing or inconsistent with `pyproject.toml`, `px sync` must fail (no implicit resolution).
* On errors, env must not be left half-updated (transactional behavior as far as possible).

---

## `px update [<pkg>…]`

**Intent**
Upgrade dependencies to newer compatible versions, then apply them.

**Preconditions**

* Project root exists.
* `px.lock` exists (otherwise this is effectively `px sync` with resolution).

**Behavior**

* With no args: attempt to update all dependencies within allowed constraints.
* With packages: update only the named dependencies (and whatever transitive changes are needed).

**Postconditions (on success)**

* `px.lock` updated to new versions.
* Project environment updated to match the new `px.lock`.
* Project is **self-consistent**.

**Failure behavior**

* On resolution failure, `px.lock` and env must remain unchanged.
* Errors must describe which constraints conflict and how to relax them.

---

## `px run <target>`

**Intent**
Run a script/task using the project environment.

**Preconditions**

* Project root exists.
* `<target>` is resolvable:

  * a script file, or
  * a named task defined in `[tool.px.scripts]` (or equivalent).

**Behavior (dev mode, default)**

* If `px.lock` is missing:

  * Resolve & install (equivalent to a minimal `px sync`), with a short note:

    * “No px.lock found, resolving dependencies…”
* If `px.lock` exists but env is out of sync:

  * Auto-sync env to `px.lock` before running (unless `--frozen` is set).
* Then execute `<target>` in the project environment.

**Behavior (strict/CI mode, e.g. `--frozen` or `CI=1`)**

* If `px.lock` missing or env out of sync:

  * Fail with clear instructions to run `px sync`.

**Postconditions (on success)**

* If any auto-sync happened:

  * Project is **self-consistent**.
* Otherwise, project consistency is unchanged.

**Failure behavior**

* If there is no px project: explicit “No px project found. Run `px init`”.
* If a dependency is missing:

  * Show Python traceback,
  * Followed by a hint: “X is not installed; add it with `px add X`.”

---

## `px test`

**Intent**
Run tests using the project environment, mirroring `px run`’s consistency behavior.

**Preconditions**

* Same as `px run`.

**Behavior**

* Same environment/sync logic as `px run` (auto-sync in dev, strict in CI).
* Invoke configured test runner (e.g. pytest, fallback to unittest).

**Postconditions (on success)**

* If any sync occurred, project is **self-consistent**.

---

## `px migrate` / `px migrate --apply`

**Intent**
Convert an existing non-px Python project into a deterministic px project.

### `px migrate` (preview)

**Preconditions**

* CWD contains legacy Python project signals:

  * `requirements.txt`, `Pipfile`, `poetry.lock`, etc.

**Behavior**

* Read legacy inputs.
* Compute proposed `pyproject.toml` + `px.lock` + env plan.
* Print a human-readable summary; **no files modified**.

**Postconditions**

* None (read-only).

---

### `px migrate --apply`

**Preconditions**

* Same as preview.
* User accepts that px will become the new project manager.

**Postconditions (on success)**

* `pyproject.toml` created or updated to include `[project]` and `[tool.px]`.
* `px.lock` created with pinned graph.
* `.px/` created with project environment.
* Legacy files (e.g. `requirements.txt`) are left intact but optionally marked as “migrated” in `[tool.px]`.
* Project is **self-consistent**.

**Failure behavior**

* On ambiguous sources (e.g. both `requirements.txt` and `poetry.lock` with conflicting graphs), `px migrate` must refuse with a clear “Why / Fix” explanation and require explicit `--from` choice.
* On failure, no partially-migrated files should be left (except logs).

---

## `px status`

**Intent**
Report project health without changing anything.

**Preconditions**

* Project root exists.

**Postconditions**

* None.
  Purely observational; may only write logs into `.px/logs/`.

---

## Global invariant for core commands

> After a successful `px init`, `px add`, `px remove`, `px sync`, `px update`, or `px migrate --apply`, the project must be in a **self-consistent** state: `pyproject.toml`, `px.lock`, and the project environment agree, and `px status` reports the environment “in sync”.

If any change breaks this invariant, it’s a UX bug, not just an implementation detail.
