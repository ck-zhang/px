## 1. Goals

* The repo root stays **visually clean**.
* px-owned files and dirs are **predictable and minimal**.
* Every command has a **bounded blast radius**: which paths it may touch is explicit.

---

## 2. Project root & detection

**Definition:**

* A directory is a **px project root** if it contains at least one of:

  * `pyproject.toml` with `[tool.px]` or
  * `px.lock`.

**Root discovery:**

* For any `px` command, start at CWD and walk up until such a directory is found.
* If none found: error with

  > “No px project found. Run `px init` in your project directory first.”

---

## 3. Allowed top-level entries (single project)

Within a project root, px **only ever creates or modifies**:

1. **User-facing, shared ecosystem:**

   * `pyproject.toml`
   * `dist/` (for built artifacts)

2. **px-specific:**

   * `px.lock`
   * `.px/` (all internal state, envs, logs, metadata)

3. **Legacy inputs (read-only, never created by px):**

   * `requirements.txt`, `Pipfile`, `poetry.lock`, etc.
     → px may *read* and, after `px migrate`, may mark as “migrated” in `[tool.px]`, but never deletes or rewrites them.

**Contract:**

* px must not create any other top-level files or directories by itself.
* px must never delete or modify files it didn’t create, except:

  * `pyproject.toml`, where it *only* touches `[project]` deps and `[tool.px]`.

---

## 4. Shape after key commands

### 4.1 After `px init` in an empty directory

Command:

```bash
mkdir myapp
cd myapp
px init
```

Tree:

```text
myapp/
  pyproject.toml
  .px/
```

* `pyproject.toml` minimal content:

  ```toml
  [project]
  name = "myapp"
  version = "0.1.0"
  requires-python = ">=3.11"
  dependencies = []

  [tool.px]
  # px-specific settings may be added later
  ```

* `.px/` is px-owned and opaque:

  ```text
  .px/
    envs/
      py3.12-linux-x86_64/<env-id>/       # deterministic project environment
    logs/
      init-<timestamp>.log                # optional, for --debug
    state.json                            # internal metadata (env id, lock hash, etc.)
  ```

**No** `px.lock` yet.
No `dist/`.
No scaffolding code.

---

### 4.2 After first dependency resolution (`px add` / `px install`)

Example:

```bash
px add requests
```

Tree:

```text
myapp/
  pyproject.toml
  px.lock
  .px/
```

Changes:

* `pyproject.toml` `dependencies` updated.
* `px.lock` created with full pinned graph.
* `.px/envs/...` updated to match the new lock.

From here on, the **stable project shape** is:

```text
pyproject.toml
px.lock
.px/
[optional] dist/
[whatever user code/layout they want]
```

---

### 4.3 After `px build`

Command:

```bash
px build
```

Tree (additions):

```text
myapp/
  pyproject.toml
  px.lock
  .px/
  dist/
    myapp-0.1.0.tar.gz
    myapp-0.1.0-py3-none-any.whl
```

Contract:

* All build artifacts must go under `dist/`.
* px does not create additional build dirs (like `build/`) unless explicitly configured.

---

### 4.4 After `px migrate --apply` from a legacy project

**Before** (example):

```text
myapp/
  requirements.txt
  app.py
```

**After:**

```text
myapp/
  pyproject.toml
  px.lock
  requirements.txt           # unchanged, treated as legacy
  app.py
  .px/
```

* `pyproject.toml` and `px.lock` are created.
* `.px/` is created as usual.
* `requirements.txt` is **left as-is**; migration state can be recorded in `[tool.px]`, e.g.:

  ```toml
  [tool.px.migration]
  from = "requirements.txt"
  status = "completed"
  ```

px must not delete or rewrite `requirements.txt` automatically.

---

## 5. Command-level shape contract

For every core command, specify **exactly what it may touch**.

### 5.1 `px init`

May:

* Create or modify `pyproject.toml` (minimal structure).
* Create `.px/` and its contents.

Must **not**:

* Create `px.lock`.
* Create `dist/`.
* Touch any user code or legacy files.

---

### 5.2 `px add`, `px remove`, `px update`

May:

* Modify:

  * `pyproject.toml` `[project].dependencies`.
  * `px.lock`.
  * `.px/envs/...` (to sync with lock).
* Create `px.lock` if missing.

Must **not**:

* Create or remove any other top-level files/dirs.

---

### 5.3 `px install`

May:

* Create `px.lock` (if installing from `pyproject.toml` alone, depending on design).
* Modify:

  * `px.lock` (if allowed),
  * `.px/envs/...`.

Should **not**:

* Modify `pyproject.toml` except in well-specified cases (ideally: not at all).

---

### 5.4 `px build`

May:

* Create or overwrite files under `dist/`.
* Log to `.px/logs/`.

Must **not**:

* Modify `pyproject.toml` or `px.lock`.
* Touch user code or legacy files.

---

### 5.5 `px publish`

May:

* Read from `dist/`.
* Log to `.px/logs/`.

Must **not**:

* Change project shape at all (no new files; no edits to existing).

---

### 5.6 `px migrate`

Preview (`px migrate`):

* **Read-only**. No shape change.

Apply (`px migrate --apply`):

May:

* Create or modify:

  * `pyproject.toml`,
  * `px.lock`,
  * `.px/`.

Must **not**:

* Delete or modify legacy files (e.g. `requirements.txt`) content.
* Create additional top-level dirs beyond `.px/` and `dist/`.

---

### 5.7 `px status`, `px env`, `px cache`, `px lock`, `px workspace`, `px why`, `px explain`

By default:

* Read-only.
* May write logs under `.px/logs/`.
* May create **no** new top-level artifacts.

(If any of these need to write something persistent — e.g. `px cache prune` — it must be constrained inside `.px/` or the global cache directory, not the project root.)

---

## 6. Workspaces (brief contract)

If/when you support workspaces, define two shapes:

### Workspace root:

```text
repo/
  pyproject.toml           # workspace config
  px.lock                  # optional workspace-level lock
  .px/
  apps/
    api/
      pyproject.toml
      px.lock
      .px/
    worker/
      pyproject.toml
      px.lock
      .px/
```

Rules:

* `px workspace *` may only:

  * modify workspace-level `pyproject.toml` + `px.lock`,
  * touch `.px/` at root and in children,
  * never create random dirs outside those.

---

## 7. Invariants (what the lead should hold the line on)

* **Invariant 1:** The only px-owned top-level paths are `px.lock`, `.px/`, and `dist/`.
* **Invariant 2:** `pyproject.toml` is the only shared file px edits, and only in `[project]` and `[tool.px]`.
* **Invariant 3:** Commands that aren’t explicitly “writey” (status/env/cache/why/explain) must not change project shape.
* **Invariant 4:** `px init` in a clean directory yields a project that looks respectable and minimal:

  ```text
  pyproject.toml
  .px/
  ```

If a change violates these, it breaks the project-shape contract.
