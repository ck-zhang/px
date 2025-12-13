## 13. Content‑addressed store (CAS)

### 13.1 Goal & scope

The px CAS is the **single source of truth** for all immutable build artifacts:

* Python runtimes,
* Built packages (site‑packages trees),
* Profiles (sets of packages + runtime).

Environments (E/WE) are **thin projections** over the CAS:

* No traditional venvs.
* No per-env copies of site-packages.
* A project/workspace/tool env is just:

  ```text
  env := profile(profile_id) + runtime(runtime_id)
  ```

linked from project/workspace-local pointers (e.g. `<root>/.px/envs/current`).

These projections are **immutable** from the user’s point of view: they are content-addressed materializations of a profile and runtime. User-initiated `pip install` cannot mutate them; dependency changes must flow through px (`px add/remove/update/sync`) so new artifacts are built into CAS and re-materialized. Envs are never “activated” directly—the supported entry points are `px run`, `px test`, and `px fmt`.

px supports two execution modes over the same profile:

* **CAS-native execution** (default for host runs): px executes *directly from the profile* without requiring an env directory at `~/.px/envs/<profile_oid>`. Runtime `sys.path` is assembled from the CAS `pkg-build` materializations (`<store>/pkg-builds/<pkg_oid>/...`), and `console_scripts` entry points are dispatched from dist metadata (via stdlib `importlib.metadata`) rather than a prebuilt `bin/` tree. Native extensions remain loadable because imports resolve to real file paths under the CAS materialized trees.
* **Materialized env execution** (compatibility / sandbox / fallback): px builds a small on-disk “site” + “bin” projection for a profile (e.g. `~/.px/envs/<profile_oid>/`) so execution can rely on PATH and standard `console_scripts` wrappers. px automatically falls back to this mode when CAS-native execution can’t safely run due to packaging/runtime quirks, or when `--sandbox` requires a materialized env.

Python still needs a writable place for runtime caches (notably `.pyc` bytecode). Because `pkg-build` trees and runtime materializations are read-only, px **redirects bytecode writes** using `PYTHONPYCACHEPREFIX`:

* Host runs: `$PX_CACHE_PATH/pyc/<profile_oid>/…` (default `~/.px/cache/pyc/<profile_oid>/…`).
* Sandbox runs: the same per-profile directory is mounted into the container and `PYTHONPYCACHEPREFIX` points at the container mount.

This keeps CAS objects immutable while allowing Python’s import caches to work normally. px may also prune older per-profile bytecode caches under `~/.px/cache/pyc` (LRU/age-based) to avoid unbounded cache growth; deleting this directory manually is safe (it will be regenerated).

The CAS must be:

* **Content‑addressed** – object identity is a digest of content + type.
* **Immutable** – objects, once stored, are never modified in place.
* **Deduplicating** – identical content is stored once.
* **Concurrency‑safe** – multi‑process use cannot corrupt the store.
* **GC‑safe** – nothing with live references is ever reclaimed.
* **Versioned** – store/index format is explicitly versioned for safe upgrades.

---

### 13.1.1 CAS format & schema versioning

The CAS layout and index are versioned:

* `index.sqlite` (or equivalent) contains a small `meta` (or `version`) table holding keys like:

  * `cas_format_version`
  * `schema_version`
  * `created_by_px_version`
  * `last_used_px_version`
* On startup, px reads this metadata:

  * If compatible with the current px version → continue.
* If incompatible → fail with a clear CAS‑level error (e.g. `PX812`) and remediation (“migrate” or “clear the CAS store”).

Backward‑incompatible changes to on‑disk layout or payload schemas must bump `cas_format_version`.

---

### 13.2 CAS nouns

The CAS introduces a few new nouns:

* **Object (O)** – an immutable blob with type and metadata.

  * `kind ∈ {source, pkg-build, runtime, profile, meta}`

* **Object ID (oid)** – hex digest (e.g. `sha256`) of canonical object bytes.

* **Store** – directory tree rooted at `~/.px/store` containing:

  * `objects/` – on-disk blobs keyed by `oid`.
  * `index.*` – metadata & reference tracking.
  * `locks/` – per-object / global lock files.

* **Profile** – a CAS object listing:

  * one runtime,
  * a set of pkg-build objects,
  * optional config (env vars, path ordering, etc.).

* **Env key (env_id)** – exactly the `profile_oid`; identifies a profile. A profile may or may not have a materialized env directory under `~/.px/envs/<profile_oid>/` (with local pointers under `<root>/.px/envs/current`). px prefers CAS-native execution and materializes env directories only when needed (sandboxing, compatibility fallback, or explicit sync/build operations).

* **Owner** – a higher-level thing that “uses” objects:

  * `runtime`, `profile`, `project-env`, `workspace-env`, `tool-env`.

* **Builder (BD)** – an internal, versioned description of the containerized build
  environment used to create `pkg-build` objects for a given `(platform, runtime)`.

  * Identified by a stable `builder_id` (e.g. `linux-x86_64-cp311-v1`).
  * Encodes OS base, compiler/toolchain, and the OS package provider
    (apt/apk/conda-forge, etc.).
  * Not a CAS object and not a user-facing noun; px may change how builders are
    provisioned as long as `builder_id` and build determinism guarantees are
    preserved.
  * Changing builder contents in a way that affects builds **must** bump
    `builder_id`, so new builds use a new build key; existing `pkg-build`
    objects remain immutable.
  * Selected via `builder_for(runtime_abi, platform)` (see §13.6.2).

The CAS is deliberately orthogonal to M/L/E and WM/WL/WE:

* Lockfiles describe **what should exist**.
* CAS + env materialization describe **how it’s realized**.

---

### 13.3 Object identity

#### 13.3.1 Digests & canonical form

Every CAS object has an identity:

```text
oid := sha256( canonical_encode(kind, payload) )
```

* `kind` is included in the encoding so two different types with the same bytes don’t alias.
* `payload` depends on the object kind (below).
* Hashes are hex‑encoded for paths and human‑visible IDs.

`canonical_encode(kind, payload)` uses a deterministic JSON encoding:

* top‑level object:

  ```json
  {
    "kind": "<kind>",
    "payload": ...
  }
  ```

* UTF‑8 encoding,

* object/map keys serialized with lexicographically sorted keys,

* no insignificant whitespace,

* lists preserve order,

* binary data in headers (if any) is represented as hex/base64 strings.

Filesystem trees in payloads are normalized:

* paths are relative and use `/` separators,
* no absolute machine‑local paths,
* entries are sorted lexicographically by path,
* timestamps and other unstable metadata are stripped.

Digest is **authoritative**:

* On read, px may rehash and must reject any on-disk blob whose digest doesn’t match its path.
* Digest never depends on machine‑local paths or timestamps.

#### 13.3.2 Object kinds

**1. `source`**

* Bytes of a downloaded wheel or sdist as delivered by the index.
* Payload:

  * Raw bytes (stored as the object body).
  * Minimal metadata baked into canonical header: `(name, version, filename, index_url, sha256_from_index)`.

**2. `pkg-build`**

* A built package tree for a specific runtime/platform/config.

  Payload includes:

  * Reference to `source_oid`.
  * Runtime ABI tag (e.g. `cp311-manylinux_x86_64`).
  * Build options hash (env vars, flags, `--no-binary`, etc.).
  * Normalized filesystem tree of:

    * `site-packages/` for this dist, including `.dist-info`.
    * `bin/` scripts produced for this dist.

* Multiple builds from the same wheel/sdist for different runtimes/platforms yield different `pkg-build` oids.

**3. `runtime`**

* A Python interpreter tree (e.g. from `python-build-standalone`).

  Payload includes:

  * Version (`3.11.8`), ABI tag, platform.
  * Normalized filesystem tree (bin, lib, include, etc.).
  * Build config hash (e.g. configure flags).

*Host-only runtimes (`PX_RUNTIME_HOST_ONLY=1`)*

* Opt-in escape hatch for local/dev use.
* The CAS entry contains only the runtime header/metadata; the archive bytes are **not** stored in CAS.
* Runtime bytes are assumed to come from the host path, so CAS immutability/replication guarantees do not apply in this mode.

**4. `profile`**

* A *logical env description*.

  Payload:

  ```text
  {
    runtime_oid: <runtime>,
    packages: [
      {name, version, pkg_build_oid},
      ...
    ],
    sys_path_order: [...],
    env_vars: {...}
  }
  ```

* Produced deterministically from:

  * `px.lock` or `px.workspace.lock` (L/WL),
  * runtime selection,
  * CAS mapping of lock entries → `pkg-build` oids.

A profile is essentially “L + runtime” rewritten into CAS IDs.

`env_vars` in the profile are applied when the env runtime is launched; they override any parent process values so that the materialized env matches the CAS-described configuration.

**5. `meta`**

* Small co-located metadata blobs, e.g. per-env manifests, CAS indices snapshots, diagnostics.
* Useful for debugging and internal invariants; not required for semantics.

---

### 13.4 Store layout

On disk, the store looks like:

```text
~/.px/store/
  objects/
    ab/
      abcdef1234...          # raw CAS blob for some oid
    cd/
      cd9876...
  index.sqlite               # metadata index + ownership cache
  locks/
    ababcdef1234...          # per-oid lock files
  tmp/
    ababcdef1234...partial   # in-progress writes
  runtimes/<oid>/            # materialized runtime tree + manifest.json (implementation detail)
  pkg-builds/<oid>/          # materialized pkg-build tree (implementation detail)
```

* Objects are sharded by two hex prefix characters to avoid giant directories.
* Filenames are the full `oid` (no semantic suffix).
* `index.sqlite` (or equivalent) is a **reconstructible index** over the store. It caches metadata and ownership, but is not the ultimate source of truth for content or liveness. It stores:

  * `meta(key PRIMARY KEY, value TEXT)` – e.g. CAS format/schema versions and px version info.
  * `objects(oid PRIMARY KEY, kind, size, created_at, last_accessed)` – cached metadata about CAS blobs.
  * `refs(owner_type, owner_id, oid, PRIMARY KEY(owner_type, owner_id, oid))` – cached ownership edges.

`owner_type` ∈ `{runtime, profile, project-env, workspace-env, tool-env}`. `owner_id` is a stable, px-level identifier, e.g.:

* `project-env:<project_root_hash>:<l_id>:<runtime>`
* `workspace-env:<workspace_root_hash>:<wl_id>:<runtime>`
* `tool-env:<tool_name>:<tool_lock_id>:<runtime>`
* `runtime:<version>:<platform>`
* `profile:<profile_oid>`

Authoritative sources:

* The bytes and canonical headers of blobs under `objects/` are the source of truth for `(oid, kind, payload)`.
* Runtime manifests under `~/.px/store/runtimes/<oid>/manifest.json` record runtime ownership metadata (version, platform, owner id) and are used when reconstructing the index/refs.
* Env materializations under `~/.px/envs/<profile_oid>/manifest.json` (and future runtime manifests) are the source of truth for which profiles/runtimes are “live roots”.
* `index.sqlite` must always be reconstructible from these authoritative sources (see §13.8.4).

#### Store immutability

* CAS objects, once created, are never modified in place.
* After successful creation, store objects under `objects/` are made read-only at the filesystem level (e.g. removing write bits) to prevent accidental mutation by tools.
* Envs are materialized via symlinks and/or `.pth` entries pointing into the store; px never creates writable paths that resolve back into `~/.px/store`.
* px may perform startup/background checks to verify and repair store permissions to maintain immutability.

---

### 13.5 Profiles & envs (no venvs)

px environments (E, WE) no longer own site‑packages; they’re just **profiles materialized on disk**.

#### 13.5.1 Profile identity

Given:

* a lock L (or WL),
* a resolved runtime `runtime_oid`,
* a mapping from each lock entry to `pkg_build_oid`,

px builds a profile payload and its `profile_oid`.

For a fixed px version, runtime, lock, and build options, `profile_oid` is deterministic.

#### 13.5.2 Env directories

Env identity is:

```text
env_id := profile_oid
```

px maintains env directories at:

```text
~/.px/envs/<profile_oid>/   # global materialization
```

and exposes project/workspace-local pointers:

```text
<project_root>/.px/envs/current -> ~/.px/envs/<profile_oid>
<workspace_root>/.px/envs/current -> ~/.px/envs/<profile_oid>
```

**Global env dir (`~/.px/envs/<profile_oid>/`) contains:**

```text
bin/                      # symlinks to bin scripts from pkg-build objects
lib/pythonX.Y/site-packages/
  (either symlinks into pkg-build trees
   or a small set of .pth files pointing into the store)
manifest.json             # per-env manifest
python -> runtime shim    # launcher that uses runtime_oid + profile_oid
```

* `manifest.json` holds:

  ```json
  {
    "profile_oid": "...",
    "runtime_oid": "...",
    "packages": [
      {"name": "...", "version": "...", "pkg_build_oid": "..."},
      ...
    ],
    "sys_path_order": [...],
    "env_vars": {...}
  }
  ```

* The `python` shim is small, px‑generated, and:

  * invokes the runtime from `runtime_oid`,
  * sets `PYTHONPATH` / `sys.path` using the packages declared in the profile in a deterministic order.
  * applies `env_vars` from the profile/env manifest, overriding any parent environment values when launching.

**Implicit base packages (`pip`, `setuptools`)**

* `manifest.json`’s `packages` list describes only locked dependencies.
* `pip` is treated as a runtime base package (provided by the selected interpreter via `ensurepip`) and is not recorded in the lock/profile.
* `setuptools` may be seeded by px as a deterministic base layer for legacy workflows and is not recorded in the lock/profile.
* `px why pip` / `px why setuptools` report them as implicit base packages.

Env materialization is idempotent and manifest-driven:

* Re-materializing an existing env ensures all files implied by `manifest.json` are present and correct.
* Stale symlinks/entries in `bin/` and `site-packages` that are no longer implied by the profile are removed.

Local symlinks under project/workspace `.px/envs/` keep your existing UX (“env belongs to project/workspace”), but all heavy content lives in `~/.px/envs` + `~/.px/store`.

#### 13.5.3 Project state integration

For a project:

* E’s **identity** is `profile_oid` (and thus its env dir).
* In `.px/state.json`, px records:

  ```json
  {
    "lock_id": "...",
    "runtime": "...",
    "profile_oid": "...",
    "env_path": "~/.px/envs/<profile_oid>"
  }
  ```

Then:

* `env_clean` (`E.l_id == L.l_id`) becomes:

  * “this profile was derived from this lock, and ~/.px/envs/<profile_oid> exists and passes integrity checks”.

For workspaces, WE is analogous; they just use WL/WE → `profile_oid`.

---

### 13.6 CAS operations & lifecycle

This section defines the key CAS operations px uses.

#### 13.6.1 `cas.ensure_source(lock_entry)`

Input:

* A lockfile node (L or WL) with source URL, expected hashes.

Behavior:

1. Download the artifact to a temporary path under `tmp/` and verify the expected hash from the index.

2. Compute `source_oid` from:

   * canonical header (`name, version, filename, index_url, expected_sha256`),
   * downloaded bytes.

3. Under lock for `source_oid`:

   * If object exists and hash matches → reuse and discard the temp file.
   * Else:

     * pack header + bytes into canonical payload,
     * write to `tmp/<source_oid>.partial`,
     * `fsync`,
     * store blob at `objects/<prefix>/<source_oid>` via atomic rename,
     * record in `objects` table.

4. Return `source_oid`.

On failure:

* No partial blob is left referenced from `objects`.
* Leftover `tmp/*.partial` is cleaned by startup self‑check.

#### 13.6.2 `cas.ensure_pkg_build(source_oid, runtime)`

Input:

* `source_oid` (wheel/sdist),
* runtime metadata (version, platform, ABI),
* build options/config.

Behavior:

1. Select a **builder** for this runtime/platform:

   * `builder_id := builder_for(runtime_abi, platform)`
   * `builder_for` returns a px-managed, containerized build environment (BD)
     with a pinned OS base, toolchain, and OS package provider (apt/apk/conda-forge/etc.).
   * `builder_for` is deterministic for a fixed px version and `(runtime_abi, platform)`.
   * Builders are internal; users cannot select or customize them directly.

2. Compute **build key** (for diagnostics/logging, not as the oid itself):

   ```text
   build_key := (source_oid, runtime_abi, builder_id, build_options_hash)
   ```

where `build_options_hash` covers user-visible build toggles (env vars, flags,
`--no-binary`, etc.). `builder_id` is always part of the key so changing the
builder changes which `pkg-build` objects are reused.

3. Materialize the build in an isolated temp dir **inside the builder environment**
   (no writes to store or envs):

   * Resolve `source_oid` into the builder.
   * Run the build backend with the requested runtime/options.
   * Let the builder use its pinned OS package provider (e.g. conda-forge) to
     install any required system libraries or headers; this is implementation
     detail and not exposed to users.
   * Produce a filesystem tree for the built dist.

4. Normalize the tree:

   * Strip timestamps and unstable paths/metadata.
   * Normalize separators, ensure relative paths.
   * Sort entries.

5. Compute `pkg_build_oid` from the canonical payload that includes `build_key`
   and the normalized tree, e.g.:

   ```text
   pkg_build_oid := sha256(
     canonical_encode("pkg-build", (build_key, normalized_fs_tree))
   )
   ```

6. Under lock for `pkg_build_oid`:

   * If object exists → reuse and discard the temp build.
   * Else:

     * tar/pack the normalized tree into canonical payload,
     * write to `tmp/<pkg_build_oid>.partial`,
     * `fsync`,
     * store as `objects/<prefix>/<pkg_build_oid>` via atomic rename,
     * record metadata in `objects`.

7. Return `pkg_build_oid`.

All per‑env site‑packages are built **once** per `(source_oid, runtime_abi, builder_id, build_options_hash)` at the CAS level. In races, multiple processes may build, but only one result wins; others observe the existing object and discard their temp builds.

Builds never run directly on the host OS; they always run inside a px-managed
builder environment. `[tool.px.sandbox]` does **not** affect which builder is
chosen or how `pkg-build` objects are produced.

#### 13.6.3 `cas.ensure_profile(L/WL, runtime)`

Input:

* A fully resolved lock (L or WL),
* chosen runtime.

Behavior:

1. For each node in L/WL:

   * call `ensure_source`,
   * call `ensure_pkg_build`,
   * collect `(name, version, pkg_build_oid)`.

2. Construct canonical profile payload:

   * `runtime_oid` (ensured via runtime installer → CAS),
   * sorted package list,
   * deterministic sys.path ordering.

3. Compute `profile_oid`.

4. Under lock for `profile_oid`:

   * If exists in CAS → reuse.
   * Else:

     * store profile payload as `profile` object.

5. Update `refs`:

   * For each `pkg_build_oid` + `runtime_oid`:

     * `INSERT OR IGNORE INTO refs(owner_type='profile', owner_id=profile_oid, oid=...)`.

Return `profile_oid`.

#### 13.6.4 `env.materialize(profile_oid)`

Input:

* `profile_oid`.

Behavior:

1. Read profile payload and referenced `runtime_oid`/`pkg_build_oid`s.

2. Verify each referenced oid exists; on missing → CAS corruption error (`PX8xx`).

3. Create or refresh `~/.px/envs/<profile_oid>/`:

   * Create or update `manifest.json`.
   * Populate `bin/` with symlinks to `bin/` scripts inside pkg-build objects.
   * Configure `lib/pythonX.Y/site-packages` as:

     * either a symlink tree into pkg-build site-packages, or
     * `.pth` pointing to each pkg-build’s site-packages in a deterministic order.
   * Remove stale files/symlinks under `bin/` and `site-packages` that are not implied by the current profile.
   * Generate `python` shim that:

     * execs runtime from `runtime_oid`,
     * sets `sys.path` exactly as per the profile.

4. Export per-project/workspace symlinks:

   * Update `<project_root>/.px/envs/current` → `~/.px/envs/<profile_oid>` for projects.
   * Update `<workspace_root>/.px/envs/current` for workspaces.

5. Update `refs`:

   * `INSERT OR IGNORE INTO refs(owner_type='<project-env|workspace-env|tool-env>', owner_id=..., oid=profile_oid)`.

---

### 13.7 Ownership & GC

The CAS uses **reference tracking** for safe cleanup.

#### 13.7.1 Ownership model

Owners:

* `runtime:<version:platform>`
* `project-env:<project_root_hash>:<l_id>:<runtime>`
* `workspace-env:<workspace_root_hash>:<wl_id>:<runtime>`
* `tool-env:<tool_name>:<tool_lock_id>:<runtime>`
* `profile:<profile_oid>`

Rules:

* `profile` owns `pkg-build` + `runtime` oids it references.
* Higher‑level owners (project-env, workspace-env, tool-env) own the `profile_oid`s they use.
* Runtime installation registers a `runtime` owner on its `runtime_oid` (runtime uninstall removes this owner).

On env deletion (e.g. project removed or lock superseded):

* px deletes the corresponding `refs(owner_type, owner_id, profile_oid)` row.
* If a profile has no refs, it can be collected *after* its grace period.

#### 13.7.2 GC algorithm

GC is a **mark-and-sweep** over objects, driven by `refs`. The index is treated as a cache, so GC has a precondition:

* GC MUST NOT run while `index.sqlite` is missing, obviously corrupt, or has failed an integrity check.
* If the index is missing or corrupt, px MUST first rebuild it from `objects/` and env/runtime manifests as specified in §13.8.4. Only after successful reconstruction may GC proceed.

1. **Mark phase**

   * Gather all `oid`s that appear in `refs` (live set).

2. **Sweep phase**

   * For each `oid` in `objects`:

     * If `oid NOT IN live_set` and older than a configured grace period:

       * Unlink `objects/<prefix>/<oid>` atomically,
       * Remove row from `objects`.

3. Optionally enforce a store size limit:

   * When store exceeds target size, prefer reclaiming the **oldest** unreferenced objects first (LRU), still subject to the grace period.

Invariants:

* An object with at least one `refs` row is never deleted.
* GC operations are transactional: a crash mid‑GC never yields an object that’s “referenced but missing” (either the object or all its refs survive).
* Size‑based GC does not violate the above invariants.
* After index reconstruction (§13.8.4), any object not reachable from env/runtime manifests (and thus not recreated in `refs`) is considered eligible for GC after the grace period; such objects can always be re-built from lockfiles if later needed.

---

### 13.8 Concurrency & crash safety

The CAS must be robust under multiple concurrent px processes.

#### 13.8.1 Per‑object lock protocol

For each `oid` being created:

* Use a lockfile `locks/<oid>.lock`:

  * Acquire via OS‑level file locking.
  * Only the holder may write `tmp/<oid>.partial` or move it to `objects/<prefix>/<oid>`.

* Creation is:

  1. Build/compute the content and canonical payload in an isolated temp location.
  2. Compute `oid`.
  3. Acquire the per‑object lock for `oid`.
  4. Write to `tmp/<oid>.partial`.
  5. `fsync` the file, then parent dir.
  6. `rename` to `objects/<prefix>/<oid>` (atomic).
  7. Insert/update row in `objects` inside a DB transaction.
  8. Release lock.

* Other processes seeing a present `objects/<prefix>/<oid>` or `objects` row treat the object as complete.

#### 13.8.2 Index transactional semantics

All mutations to `index.sqlite` happen inside transactions:

* For each env/profile/runtime install:

  * `BEGIN IMMEDIATE`
  * Write rows in `objects` and `refs`.
  * `COMMIT`.

On crash:

* Store may contain unreferenced objects (safe).
* `refs` table always describes a self‑consistent world (no half‑written rows).

#### 13.8.3 Startup / self‑check

Optional background health checks:

* Sweep `tmp/*.partial` and delete any leftover partials.
* Sample existing objects and verify their digests; delete corrupt blobs and remove `refs` entries that point only to missing objects, prompting rebuild on next use.
* Check CAS format/schema version in `index.sqlite` and fail cleanly on incompatibility.
* If `index.sqlite` is missing or fails integrity checks, reconstruct it from the store + manifests as per §13.8.4 before any GC/cleanup.

#### 13.8.4 Index reconstruction & GC gating

* Create a fresh index (schema + meta).
* Rebuild `objects` by walking `~/.px/store/objects/**`, reading canonical headers, and recording `(oid, kind, size, timestamps)`.
* Rebuild `refs` from env manifests under `~/.px/envs`, adding `profile` → `profile/runtime/pkg-build` edges.
* Optionally reconstruct higher-level owners (project-env, workspace-env, tool-env) from `.px/state.json` for richer diagnostics.
* Mark the index as healthy; GC and size-based cleanup MUST only run against a healthy index.
* Consequences: objects not reachable from manifests may be GC’d after the grace period but are reproducible from lockfiles.

---

### 13.9 Remote CAS

The current implementation only supports a local on-disk CAS. Remote backends (HTTP/S3/etc.) are not part of the current design and may be revisited in the future.

---

### 13.10 Error model & observability

CAS introduces a new error family (example codes):

* `PX800` – CAS object missing/corrupt.

  * `Why`:

    * `• Expected CAS object abc... is missing or has an invalid digest.`

  * `Fix`:

    * `• Run 'px sync' to rebuild environments from lockfiles.`
    * `• Or clear the CAS store and rerun if corruption persists.`

* `PX810` – CAS store write failure (disk full, permissions, etc.).

* `PX811` – CAS index corruption.

* `PX812` – CAS format/schema incompatible with this px version.

All CAS operations must:

* Emit deterministic log lines about:

  * object creation,
  * cache hits,
  * GC decisions,

* Respect `--json` and non‑TTY rules (no spinners).

* Be safe to retry on `PX800`/`PX810` unless explicitly documented otherwise.

Implementations may also expose basic CAS metrics (store size, per‑kind hit/miss, GC reclaimed bytes) where useful.
