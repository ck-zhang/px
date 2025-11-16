## 1. Core philosophy

**Tagline to give your lead:**

> Command layout inspired by Go, failure UX inspired by Cargo.
> Few obvious verbs, strict structure for all output, and explicit determinism.

### 1.1 Design rules

1. **Week-one actions are top-level verbs**

   * Anything a new user needs in the first week is `px <verb>`, no nouns:

     * `px init`, `px add`, `px remove`, `px sync`, `px update`, `px run`, `px test`.

2. **Concepts are nouns, not taxonomies**

   * Only introduce a noun if it’s a real domain concept:

     * `env`, `cache`, `lock`, `workspace`.
   * No abstract buckets (`project`, `workflow`, `quality`, `output`, `infra`, `store`).

3. **One voice, three shapes of output**

   * Success: what happened → what changed → optional next step.
   * Info: current state, in a compact, structured format.
   * Problem: what failed → short “why” bullets → copy-pasteable fix.

4. **Determinism is surfaced, not hidden**

   * Lockfile + env path are treated as first-class outputs of key commands.
   * `px migrate` is the canonical “make this project deterministic” operation.

5. **Always automation-friendly**

   * Stable exit codes, consistent flags (`-q`, `-v`, `--debug`, `--json`).
   * No prompts under `CI=1` / `PX_NONINTERACTIVE=1`.

---

## 2. Top-level command set

This is what `px --help` should show. No other top-levels.

### 2.1 Week-one verbs

```text
px init       Create a new px project and environment
px add        Add dependencies and update lock/env
px remove     Remove dependencies and update lock/env
px sync    Sync environment from lockfile
px update     Update dependencies and lockfile
px run        Run a named task or script
px test       Run tests
```

### 2.2 Project workflow & packaging

```text
px fmt        Format code
px lint       Lint code
px tidy       Clean up project artifacts
px build      Build distributable artifacts
px publish    Publish a built artifact
px migrate    Convert an existing project to deterministic px
px status     Show project/env/lock status at a glance
```

### 2.3 Noun-scoped areas

```text
px env        Inspect and manage the runtime environment
px cache      Inspect and manage caches
px lock       Inspect and manage the lockfile
px workspace  Work with multi-project workspaces
```

### 2.4 Power / investigation tools

```text
px explain    Show detailed explanation for a recorded issue/resolution
px why        Explain why a given package is present
```

Everything else (old `project`, `workflow`, `quality`, `output`, `infra`, `store`) disappears from primary UX. If you care, keep them as backward-compatible aliases, but don’t show them in `--help`.

---

## 3. Command semantics (designed, not inherited)

### 3.1 Week-one verbs

**`px init`**

* Create pyproject/metadata if missing.
* Create or select a project env.
* Output:

  ```text
  ✔ Project initialized in ./myapp
  → Using Python 3.12.1 in .venv
  Tip: Add a dependency with `px add <name>`
  ```

**`px add <pkg>…`**

* Update dependency spec.
* Resolve versions.
* Update lock & env.
* On success:

  ```text
  ✔ Added requests 2.32.3
  → Updated px.lock and .venv (2 packages installed)
  Tip: Run `px test` to verify everything still passes.
  ```

**`px remove <pkg>…`**

* Remove from spec, update lock & env.
* Similar shape to `add`.

**`px sync [--frozen]`**

* Install from lockfile only.
* `--frozen` = fail if lock and spec diverge.

**`px update [<pkg>…]`**

* Re-resolve dependencies, update lock & env.
* `px update` (no args) = full upgrade.
* `px update foo` = constrained upgrade for `foo`.

**`px run <task>`**

* Run a named task (script/entrypoint).
* Always uses the px-managed environment.

**`px test [pattern]`**

* Run tests in the px env (`pytest`, `unittest`, whatever you choose).
* Output just enough to see pass/fail; test runner detail is delegated to the runner.

---

### 3.2 Workflow & packaging verbs

**`px fmt`**

* Apply configured formatter(s) (e.g. `ruff format`, `black`, etc.).
* Quiet success:

  ```text
  ✔ Formatted source files
  ```

**`px lint`**

* Run linters over the codebase.

**`px tidy`**

* Clean `.pyc`, build dirs, temporary artifacts, etc.
* On workspace: `px workspace tidy` does multi-project equivalent.

**`px build`**

* Build sdist/wheel (whatever you support).
* Outputs paths to produced artifacts.

**`px publish`**

* Upload artifacts to configured index.

**`px migrate`**

* Purpose: “Take what’s here and produce a deterministic px setup (pins + env path)”.
* `px migrate` → preview plan (non-destructive).
* `px migrate --apply` → write lock, set env path, maybe mark legacy files as ignored.

Preview example:

```text
✔ Migration plan for this project:

From:
  • requirements.txt (23 packages)
  • Existing virtualenv at .venv

To:
  • px.lock with 37 pinned dependencies
  • Environment: .px/envs/py3.12-linux-x86_64/abcd1234

Next:
  • Apply this plan: `px migrate --apply`
  • See full details: `px migrate --verbose`
```

Apply example:

```text
✔ Migrated project to px

Changed:
  • Created px.lock (37 pinned dependencies)
  • Using env at .px/envs/py3.12-linux-x86_64/abcd1234

Next:
  • Run tests: `px test`
  • Optionally remove legacy requirements.txt when you’re satisfied.
```

**`px status`**

* High-level state snapshot:

  ```text
  Project: myapp
  Python: 3.12.1 (.venv/bin/python)

  State:
    • Lockfile: px.lock (up to date)
    • Environment: in sync with px.lock
    • Updates: 3 newer versions available (run `px update`)

  Next:
    • Run tests: `px test`
  ```

This is the “what is going on?” command.

---

### 3.3 Noun scopes

**`px env`**

```text
px env info          Show current env, Python version, and key paths
px env python <ver>  Select/create Python version for this project
px env paths         List env-related paths (env dir, cache, lock, etc.)
```

**`px cache`**

```text
px cache path        Show cache directory/directories
px cache stats       Show cache usage summary
px cache prune       Remove unused or stale cache entries
px cache prefetch    Prefetch and cache artifacts for offline use   # from old `store prefetch`
```

**`px lock`**

```text
px lock diff         Show differences between resolved state and px.lock
px lock upgrade      Refresh lockfile without installing
```

(Optionally: `px update --lock-only` as user-friendly alias.)

**`px workspace`**

```text
px workspace list      List projects in the workspace
px workspace verify    Check workspace consistency
px workspace sync   Install all workspace dependencies
px workspace tidy      Cleanup workspace-wide artifacts
```

---

### 3.4 Power tools

**`px explain <issue-id>`**

* When a complex failure occurs (resolution conflict, env mismatch), px prints an `issue id`.
* `px explain <id>` shows deep details:

  * dependency graph slice,
  * conflicting constraints,
  * which versions were considered.

**`px why <pkg>`**

* Explain why a package is present:

  ```text
  urllib3 2.2.2 is required because:
    • requests 2.32.3 depends on urllib3>=1.21.1,<3
    • px.lock pins urllib3 to 2.2.2
  ```

---

## 4. Output structure & flags

### 4.1 Output

* Success:

  * 1 status line (`✔ …`).
  * 1 line of changes (`→ …`) if relevant.
  * 0–1 “Tip” line.

* Errors:

  ```text
  ✗ <summary>

  Why:
    • …

  Fix:
    • …

  For more detail: re-run with `--debug`.
  ```

* Info: tabular/list, no prose.

No default stack traces. No hairball resolver logs unless `--debug`.

### 4.2 Flags (consistent across commands)

* `-q`, `--quiet`: only critical output.
* `-v`, `-vv`: add more context (e.g. indexes, selected versions, timing).
* `--debug`: full trace, internals; also logs to a file and prints its path.
* `--json`: structured machine-readable output (where it makes sense).
* Respect `CI=1` / `PX_NONINTERACTIVE=1`: no prompts, fail explicitly instead.

---

## 5. Environment & determinism rules

* Default: one px-managed env per project.
* Installing into system Python always warns unless explicitly configured.
* `px env info` must always include:

  * interpreter path,
  * version,
  * env type,
  * whether it’s synced with `px.lock`.

Deterministic identity of an env is `(lock hash, platform, Python version)`, and you *surface* that in `px sync`, `px update`, `px migrate`.

---

This is a **designed** UX: a small, coherent command set, clear mental model, strict output structure, and a story around determinism and migration. Not “we conformed what you had,” but “here is what px should be if we weren’t afraid to break things.”
