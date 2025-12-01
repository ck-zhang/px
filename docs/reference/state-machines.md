# State Machines

Projects and workspaces share the same shape: Manifest (M/WM), Lock (L/WL), and Env (E/WE). Command contracts are defined as transitions over these state machines.

Storage/materialization details for envs and artifacts live in the [Content Addressable Store](../design/content-addressable-store.md) design note.

## Project state machine

### Core entities

* **M (Manifest)** – parsed `pyproject.toml`:

  * `[project].dependencies`
  * `[tool.px]` (including any px-specific dep config)
  * project-level Python constraints

* **L (Lock)** – parsed `px.lock`:

  * full resolved dependency graph
  * runtime (python version/tag)
  * platform tags
  * manifest fingerprint

* **E (Env)** – the current project environment under `.px/envs/...`:

  * pointer to `L` it was built from (lock hash / ID)
  * runtime used
  * px metadata describing installed packages (px does not rescan site-packages)

Tool environments are separate and are not part of this state machine.

### Identity and fingerprints

**Manifest fingerprint (`mfingerprint`)**

* Inputs: `[project].dependencies`, `[tool.px].dependencies` extensions/groups, relevant Python/version markers.
* Output: deterministic hash (e.g. `sha256`).

**Lock identity**

* `px.lock` stores `mfingerprint`, a lock ID (`l_id`, hash of full lock content), runtime, and platform info. L is valid for exactly one `mfingerprint`.

**Env identity**

* Each env stores `l_id`, runtime version/ABI, and platform so px can answer “is E built from this L?” without scraping site-packages.

### Derived flags

* `manifest_exists` – `pyproject.toml` present and parseable with `[project]`.
* `lock_exists` – `px.lock` present and parseable.
* `env_exists` – px metadata shows at least one env for this project.

Assuming all three parse cleanly:

* `manifest_clean` – `lock_exists` and `L.mfingerprint == mfingerprint(M)` **and** `detect_lock_drift` reports no drift (version/mode/project/python mismatches are NeedsLock even when fingerprints match).
* `env_clean` – `env_exists` and `E.l_id == L.l_id`.

Core invariant:

```text
project_consistent := manifest_clean && env_clean
```

### Canonical states

* **Uninitialized**

  * `manifest_exists == false`
  * No lock, no env.
  * Only `px init` is allowed; others error “no px project found”.

* **InitializedEmpty**

  * `manifest_exists == true`
  * `[project].dependencies` empty
  * `lock_exists == true` with empty graph and correct `mfingerprint`
  * `env_exists == true`, `env_clean == true`
  * Equivalent to `Consistent`

* **NeedsLock**

  * `manifest_exists == true`
  * (`lock_exists == false`) or (`manifest_clean == false`)
  * Typical cause: manifest edited manually or `px.lock` deleted.

* **NeedsEnv**

  * `manifest_clean == true`
  * (`env_exists == false`) or (`env_clean == false`)
  * Typical cause: first install on a machine or user wiped `.px/envs`.

* **Consistent**

  * `manifest_clean == true`
  * `env_clean == true`

### Command transitions

Treat commands as transitions between canonical states. When a command fails, it must not leave partial writes.

#### `px init`

* **Start**: `Uninitialized`
* **Behavior**: create minimal manifest; create empty lock for chosen runtime; create empty env matching lock.
* **End**: `InitializedEmpty` (also `Consistent`). Refuses if another tool clearly owns `pyproject.toml`.

#### `px add` / `px remove`

* **Start**: any with `manifest_exists == true` (`InitializedEmpty`, `Consistent`, `NeedsLock`, `NeedsEnv`).
* **Behavior**: modify manifest; resolve deps → new lock; build env from new lock.
* **End**: `Consistent`. Atomic: on resolver failure, no changes.

#### `px sync [--frozen]`

* **Start**: any with `manifest_exists == true`.
* **Dev behavior**:

  * If `lock_exists == false` or `manifest_clean == false`: resolve deps from manifest → new lock.
  * Ensure env built from current lock (create/replace if needed).

* **Frozen/CI**:

  * If `lock_exists == false` or `manifest_clean == false`: fail; never resolve.
  * Else: only fix env if stale.

* **End**: `Consistent` on success.

#### `px update [<pkg>…]`

* **Start**: `manifest_exists == true` and `lock_exists == true` (typically `Consistent`).
* **Behavior**: compute new lock with newer versions (bounded by constraints); build env from new lock.
* **End**: `Consistent`. On resolution failure, no changes.

#### `px run` / `px test`

* **Dev allowed start**: `Consistent`, `NeedsEnv`.
* **Frozen/CI allowed start**: only `Consistent`.
* **Behavior (dev)**: if `NeedsEnv`, rebuild env from existing lock (no re-resolution); run target via env.
* **Behavior (frozen/CI)**: fail if not `Consistent`; never repairs envs.
* **End**: manifests and locks unchanged; env may be rehydrated → `Consistent` or unchanged.

#### `px fmt`

* **Start**: any with `manifest_exists == true`.
* **Behavior**: operates only on code and tool envs; never touches manifest, lock, or project env in dev or CI.
* **End**: project state unchanged.

#### `px status`

* **Start**: any (including `Uninitialized`).
* **Behavior**: report manifest/lock/env presence and cleanliness plus derived state.
* **End**: unchanged.

#### `px migrate` / `px migrate --apply`

* `px migrate` – read-only planning from legacy inputs; no writes.
* `px migrate --apply` – creates manifest, lock, and env from legacy inputs; on success ends `Consistent`; must not leave partial migration on failure.

### Error and hint logic

* Only report “Manifest drift detected” when `manifest_exists == true`, `lock_exists == true`, and `manifest_clean == false`, and only from commands that do not fix M/L (`run`, `test`, `fmt`, `status`).
* Missing-import hinting in `px run`: if the missing module is a direct dependency in M, suggest `px sync`; if not present in M/L, suggest `px add <pkg>`.

### Why this model

With entities (M/L/E), fingerprints (`mfingerprint`, `l_id`), flags (`manifest_clean`, `env_clean`), canonical states, and per-command start/end states, implementations and tests can enforce deterministic behavior (e.g., `px fmt` never rebuilding envs, `px run` never updating locks).

### Command invariants (project)

Legend: M = manifest (`pyproject.toml`), L = lock (`px.lock`), E = env (project env); States = { U = Uninitialized, IE = InitializedEmpty, NL = NeedsLock, NE = NeedsEnv, C = Consistent }.

| Command              | Allowed start states | Writes M?                 | Writes L?                         | Writes E?                | Required end state (on success) | Notes                                                                     |
| -------------------- | -------------------- | ------------------------- | --------------------------------- | ------------------------ | ------------------------------- | ------------------------------------------------------------------------- |
| `px init`            | U                    | Yes                       | Yes                               | Yes                      | IE (also C)                     | Refuses if another tool clearly owns `pyproject.toml`.                    |
| `px add`             | IE, C, NL, NE        | Yes                       | Yes                               | Yes                      | C                               | Atomic: on resolver failure, no changes.                                  |
| `px remove`          | IE, C, NL, NE        | Yes                       | Yes                               | Yes                      | C                               | Only direct deps may be removed.                                          |
| `px sync`            | IE, C, NL, NE        | NL: L only; others: maybe | Yes (in dev or when lock missing) | Yes (if E dirty/missing) | C                               | Under `--frozen`/CI: never writes L; only repairs E if M/L already clean. |
| `px update`          | Any with M+L present | No                        | Yes                               | Yes                      | C                               | On resolution failure, no changes.                                        |
| `px migrate`         | Any (legacy or U)    | No                        | No                                | No                       | N/A                             | Read-only planning; prints proposal.                                      |
| `px migrate --apply` | Any (legacy or U)    | Yes                       | Yes                               | Yes                      | C                               | Must not leave partial migration on failure.                              |

| Command     | Allowed start states (dev) | Allowed start states (CI/`--frozen`) | Writes M? | Writes L? | Writes E? (project) | Required end state (on success) | Notes                                                     |
| ----------- | -------------------------- | ------------------------------------ | --------- | --------- | ------------------- | ------------------------------- | --------------------------------------------------------- |
| `px run`    | C, NE                      | C                                    | No        | No        | Dev: NE→C; C→C      | C or unchanged                  | In dev, may repair E; in CI, never repairs E.             |
| `px test`   | C, NE                      | C                                    | No        | No        | Dev: NE→C; C→C      | C or unchanged                  | Same rules as `px run`.                                   |
| `px fmt`    | Any with `manifest_exists` | Any with `manifest_exists`           | No        | No        | No                  | Unchanged                       | Operates only on code and tool envs; never touches M/L/E. |
| `px status` | Any                        | Any                                  | No        | No        | No                  | Unchanged                       | Purely introspective.                                     |
| `px why`    | Any with `manifest_exists` | Any with `manifest_exists`           | No        | No        | No                  | Unchanged                       | Purely introspective.                                     |

## Workspace state machine

### Core entities

* **WM (Workspace Manifest)** – derived from `[tool.px.workspace]` in workspace root `pyproject.toml`:

  * list of member project paths (relative to workspace root),
  * optionally shared Python constraints, index config, etc.

  Each member project itself has a project manifest M in its own `pyproject.toml`.

* **WL (Workspace Lock)** – `px.workspace.lock`:

  * full resolved dependency graph across all members,
  * runtime (Python version/tag) for the workspace,
  * platform tags,
  * mapping from graph nodes to owning member project (or “external”),
  * workspace manifest fingerprint.

* **WE (Workspace Env)** – environment under `.px/...` at the workspace root:

  * pointer to WL it was built from (lock hash / ID),
  * runtime used,
  * px metadata describing installed packages.

### Identity and fingerprints

**Workspace manifest fingerprint (`wmfingerprint`)**

* Inputs: workspace members list; for each member, `[project].dependencies` (plus any `[tool.px].dependencies` extensions/groups) and relevant markers.
* Output: deterministic hash (e.g. `sha256`).

**Workspace lock identity**

* `px.workspace.lock` stores `wmfingerprint`, workspace lock ID (`wl_id`, hash of full lock), runtime, and platform info. WL is valid for exactly one `wmfingerprint`.

**Workspace env identity**

* Each workspace env stores `wl_id`, runtime version/ABI, and platform so px can answer “is WE built from this WL?” without scraping site-packages.

### Derived flags

* `w_manifest_exists` – `[tool.px.workspace]` present and parseable.
* `w_lock_exists` – `px.workspace.lock` present and parseable.
* `w_env_exists` – px metadata shows at least one workspace env for this root.

Assuming all three parse cleanly:

* `w_manifest_clean` – `w_lock_exists` and `WL.wmfingerprint == wmfingerprint(WM)`.
* `w_env_clean` – `w_env_exists` and `WE.wl_id == WL.wl_id`.

Core invariant:

```text
workspace_consistent := w_manifest_clean && w_env_clean
```

### Canonical states

* **WUninitialized**

  * `w_manifest_exists == false`
  * No workspace lock, no workspace env.

* **WInitializedEmpty**

  * `w_manifest_exists == true`
  * Member list may be empty or contain projects with empty deps.
  * `w_lock_exists == true` with correct `wmfingerprint` and empty/degenerate graph.
  * `w_env_exists == true`, `w_env_clean == true`.

* **WNeedsLock**

  * `w_manifest_exists == true` and (`w_lock_exists == false` or `w_manifest_clean == false`).
  * Typical cause: member manifests changed, or `px.workspace.lock` removed.

* **WNeedsEnv**

  * `w_manifest_clean == true` and (`w_env_exists == false` or `w_env_clean == false`).
  * Typical cause: first install on a machine or user wiped workspace env.

* **WConsistent**

  * `w_manifest_clean == true`
  * `w_env_clean == true`.

### Command mapping in workspace context

Existing verbs operate over WM/WL/WE when routed via a workspace:

* **`px sync` (from workspace root or member)**

  * Start: any with `w_manifest_exists == true`.
  * Dev: if `WNeedsLock`, resolve union of member manifests → new WL; ensure WE built from WL.
  * Frozen/CI: if `WNeedsLock`, fail; if `WNeedsEnv`, rebuild WE only.
  * End: `WConsistent` on success.

* **`px update` (workspace root or member)**

  * Requires `w_manifest_exists` and `w_lock_exists`.
  * Updates WL (versions within constraints) and WE.
  * End: `WConsistent`.

* **`px add` / `px remove` (from member)**

  * Modify that member’s manifest.
  * Then resolve across all members → WL; rebuild WE.
  * End: `WConsistent`.

* **`px run` / `px test` (from member)**

  * Dev allowed start: `WConsistent`, `WNeedsEnv`; may repair WE (no re-resolution).
  * Frozen/CI allowed start: only `WConsistent`; never repairs WE.

* **`px status`**

  * At workspace root: report workspace state and list members plus whether their manifests were included in `wmfingerprint`.
  * In a member: report workspace state plus whether that project’s manifest is included.

### Projects as workspace members

* A project is a workspace member if its path is listed (or matches a glob) in `[tool.px.workspace].members` and the workspace root is an ancestor.
* In workspace context:

  * The project’s manifest M remains authoritative for that project’s metadata.
  * Per-project lock/env (`px.lock`, project env) are ignored for resolution/materialization.
  * All dependency and env operations go through WL/WE.

* Outside workspace context (no applicable workspace root), the project uses the project state machine with `px.lock` and its own env.

This guarantees a single authority for deps/env at a time: either the project state machine (standalone) or the workspace state machine (for members).
