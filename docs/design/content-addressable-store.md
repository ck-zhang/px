## 13. Content‑addressed store (CAS)

### 13.1 Goal & scope

The px CAS is the **single source of truth** for all immutable build artifacts:

* Python runtimes,
* Built packages (site‑packages trees),
* Profiles (sets of packages + runtime).

Environments (E/WE) are **thin projections** over the CAS:

* No traditional venvs.
* No per‑env copies of site‑packages.
* A project/workspace/tool env is just:

  ```text
  env := profile(profile_id) + runtime(runtime_id)
  ```

linked from `.px/envs/...`.

The CAS must be:

* **Content‑addressed** – object identity is a digest of content + type.
* **Immutable** – objects, once stored, are never modified in place.
* **Deduplicating** – identical content is stored once.
* **Concurrency‑safe** – multi‑process use cannot corrupt the store.
* **GC‑safe** – nothing with live references is ever reclaimed.
* **Remote‑ready** – optional push/pull of objects between machines.
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

* **Env key (env_id)** – exactly the `profile_oid`; identifies a **profile materialization** under `.px/envs`.

* **Owner** – a higher-level thing that “uses” objects:

  * `runtime`, `profile`, `project-env`, `workspace-env`, `tool-env`.

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
  index.sqlite               # metadata + ownership
  locks/
    ababcdef1234...          # per-oid lock files
  tmp/
    ababcdef1234...partial   # in-progress writes
```

* Objects are sharded by two hex prefix characters to avoid giant directories.
* Filenames are the full `oid` (no semantic suffix).
* `index.sqlite` (or similar) stores:

  * `meta(key PRIMARY KEY, value TEXT)` – e.g. CAS format/schema versions and px version info.
  * `objects(oid PRIMARY KEY, kind, size, created_at, last_accessed)`
  * `refs(owner_type, owner_id, oid, PRIMARY KEY(owner_type, owner_id, oid))`

`owner_type` ∈ `{runtime, profile, project-env, workspace-env, tool-env}`. `owner_id` is a stable, px-level identifier, e.g.:

* `project-env:<project_root_hash>:<l_id>:<runtime>`
* `workspace-env:<workspace_root_hash>:<wl_id>:<runtime>`
* `tool-env:<tool_name>:<tool_lock_id>:<runtime>`
* `runtime:<version>:<platform>`
* `profile:<profile_oid>`

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
    ]
  }
  ```

* The `python` shim is small, px‑generated, and:

  * invokes the runtime from `runtime_oid`,
  * sets `PYTHONPATH` / `sys.path` using the packages declared in the profile in a deterministic order.

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
* Leftover `tmp/*.partial` is cleaned by startup self‑check / `px doctor`.

#### 13.6.2 `cas.ensure_pkg_build(source_oid, runtime)`

Input:

* `source_oid` (wheel/sdist),
* runtime metadata (version, platform, ABI),
* build options/config.

Behavior:

1. Compute **build key** (for diagnostics/logging, not as the oid itself):

   ```text
   build_key := (source_oid, runtime_abi, build_options_hash)
   ```

2. Materialize the build in an isolated temp dir (no writes to store or envs):

   * resolve `source_oid`,
   * run build backend with the requested runtime/options,
   * produce a filesystem tree.

3. Normalize the tree:

   * strip timestamps and unstable paths/metadata,
   * normalize separators, ensure relative paths,
   * sort entries.

4. Compute `pkg_build_oid` from the canonical payload that includes `build_key` and the normalized tree, e.g.:

   ```text
   pkg_build_oid := sha256( canonical_encode("pkg-build", (build_key, normalized_fs_tree)) )
   ```

5. Under lock for `pkg_build_oid`:

   * If object exists → reuse and discard the temp build.
   * Else:

     * tar/pack the normalized tree into canonical payload,
     * write to `tmp/<pkg_build_oid>.partial`,
     * `fsync`,
     * store as `objects/<prefix>/<pkg_build_oid>` via atomic rename,
     * record metadata in `objects`.

6. Return `pkg_build_oid`.

All per‑env site‑packages are built **once** per `(source, runtime, options)` at the CAS level. In races, multiple processes may build, but only one result wins; others observe the existing object and discard their temp builds.

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

GC is a **mark‑and‑sweep** over objects, driven by `refs`:

1. **Mark phase**

   * Gather all `oid`s that appear in `refs` (live set).

2. **Sweep phase**

   * For each `oid` in `objects`:

     * If `oid NOT IN live_set` and older than a configured grace period:

       * Delete `objects/<prefix>/<oid>` atomically,
       * Remove row from `objects`.

3. Optionally enforce a store size limit:

   * When store exceeds target size, prefer reclaiming the **oldest** unreferenced objects first (LRU), still subject to the grace period.

Invariants:

* An object with at least one `refs` row is never deleted.
* GC operations are transactional: a crash mid‑GC never yields an object that’s “referenced but missing” (either the object or all its refs survive).
* Size‑based GC does not violate the above invariants.

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

Optional `px doctor` / background health checks:

* Sweep `tmp/*.partial` and delete any leftover partials.
* Sample existing objects and verify their digests; delete corrupt blobs and remove `refs` entries that point only to missing objects, prompting rebuild on next use.
* Check CAS format/schema version in `index.sqlite` and fail cleanly on incompatibility.

---

### 13.9 Remote CAS (optional, but spec’d)

The CAS is designed to support a remote backend without changing the high‑level semantics.

#### 13.9.1 Backend abstraction

Define an abstract `StoreBackend`:

* `has(oid) -> bool`
* `get(oid) -> bytes`
* `put(oid, bytes) -> ()`
* `list(kind?, prefix?) -> [oid]`

px ships with:

* `LocalBackend` (the on‑disk store above).
* Optionally (later): `RemoteBackend` (HTTP, S3, etc.).

#### 13.9.2 Push / pull behavior

* On `cas.ensure_*`:

  * Check local first.
  * If missing and remote configured:

    * Try `remote.get(oid)` and, if present, populate local store (subject to digest verification).
    * Else build/download and then `remote.put(oid)` if policy allows.

* For any `remote.get(oid)`:

  * px must rehash the bytes and reject them if the digest doesn’t match `oid` (treat as `PX800`‑style corruption).

* Remote errors:

  * Never corrupt local store.
  * px surfaces them as non-fatal (falls back to local builds if possible) or as clear CAS‑level errors (`PX81x`).

The digest is always authoritative; remote is a cache/replica, not a different source of truth.

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

* `PX81x` – Remote CAS failures (network, auth, integrity).

All CAS operations must:

* Emit deterministic log lines about:

  * object creation,
  * cache hits,
  * GC decisions,

* Respect `--json` and non‑TTY rules (no spinners).

* Be safe to retry on `PX800`/`PX810`/`PX81x` unless explicitly documented otherwise.

Implementations may also expose basic CAS metrics (store size, per‑kind hit/miss, GC reclaimed bytes) where useful.
