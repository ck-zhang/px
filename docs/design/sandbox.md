## 14. Sandbox (SBX)

### 14.1 Goal & scope

Sandboxing adds a second deterministic layer on top of px envs:

* **Env layer (existing)** – runtime + Python packages + px env layout (CAS profile).
* **Sandbox layer (new)** – base OS + system libraries + containerized execution.
* **Portable app packaging (built on sandbox)** – single‑file `.pxapp` bundles that serialize a sandboxed app (base OS, system deps, Python runtime, env, and project code) for direct `px run` execution.

The sandbox layer exists to support:

* Packages and apps that depend on **system libraries** (`libpq`, `libjpeg`, `libxml2`, etc.).
* **Containerized execution** (dev & CI) that matches production images.
* Deterministic, repeatable **deployable images** (`px pack image`) without exposing Docker complexity.
* Deterministic, self‑contained **portable bundles** (`px pack app` → `.pxapp`) that can be executed on any machine with `px` and a compatible container backend, without a project or workspace.

Sandbox is **optional**:

* Default workflow uses host OS + px envs (`px run`, `px test`).
* Sandbox flows are opt‑in (`px run --sandbox`, `px test --sandbox`, `px pack image`, `px pack app`).
* `.pxapp` execution via `px run <bundle.pxapp>` is opt‑in and does not require a project/workspace.

Sandbox is **derived** from the env profile and a small **sandbox manifest**; it does not mutate project/workspace state. `.pxapp` bundles are exports of this derived sandbox state plus an app code snapshot; they do not extend the state machine.

### 14.1.1 Build vs runtime sandboxing

Sandbox (SBX) is a **runtime** concept: it describes the OS + system libraries
used when running code (`px run --sandbox`, `px test --sandbox`, `px pack image`,
`px pack app`).

Native builds are handled separately by **builder** environments (BD):

* Builders are px-managed container images used only for creating `pkg-build`
  CAS objects (see §13.6.2).
* Builders may use an internal OS package provider (e.g. conda-forge) to install
  system libraries and headers needed to compile Python packages.
* Builders are not configurable by users and are not controlled by
  `[tool.px.sandbox]`.

Invariants:

* `[tool.px.sandbox]` **never** affects CAS `pkg-build` contents or builder
  selection; it only affects runtime sandbox images (SI) and portable bundles.
* Changing sandbox base or capabilities changes `sbx_id` and sandbox images,
  but does not invalidate or recompute `pkg-build` CAS objects.

---

### 14.2 Sandbox nouns

Sandbox introduces a small number of new nouns:

* **Sandbox base (SB)** – a minimal OS filesystem snapshot with fixed system packages.

  * Examples: `px/base:debian-12-py311`, `px/base:alpine-3.20-py312`.
  * Stored as a CAS object with a unique `base_os_oid`.

* **Sandbox capability (SC)** – a high‑level requirement for system libraries.

  * Examples: `postgres`, `mysql`, `imagecodecs`, `xml`, `ldap`, `ffi`, `curl`.
  * px maps capabilities → distro‑specific packages per base.

* **Sandbox manifest (SM)** – per‑project/workspace declarative config:

  ```toml
  [tool.px.sandbox]
  base = "debian-12"      # logical base name, px resolves to base_os_oid
  auto = true             # allow px to auto-infer capabilities

  [tool.px.sandbox.capabilities]
  postgres    = true
  imagecodecs = true
  ```

* **Sandbox definition (SD)** – the **effective** sandbox configuration for one env profile:

  ```text
  SD := (base_os_oid, capabilities_set, profile_oid, sbx_version)
  ```

* **Sandbox ID (`sbx_id`)** – stable identifier for an SD:

  ```text
  sbx_id := sha256( canonical_encode(
    base_os_oid,
    sorted(capabilities_set),
    profile_oid,
    sbx_version
  ))
  ```

* **Sandbox image (SI)** – a container/OCI image materialized from SD:

  * Fully determined by `sbx_id` (base OS + system packages + Python runtime + env).
  * Used ephemerally for `px run --sandbox` / `px test --sandbox`, and as the base for deployable artifacts produced by packing commands (`px pack image`, `px pack app`).

* **Sandbox app bundle (`.pxapp`)** – a **portable, single‑file** serialization of a sandboxed app:

  * Contains the sandbox base, system packages, Python runtime, env, and a snapshot of app code, plus minimal runtime metadata.
  * Executed via `px run <bundle.pxapp>`; does not require a project/workspace.

Sandbox is **orthogonal** to:

* CAS (objects, profiles, env materialization).
* Project/workspace state machines (M/L/E and WM/WL/WE).

Sandbox consumes `profile_oid` as an input; it never changes locks or envs. `.pxapp` bundles are consumers of sandbox state and do not extend the state machines.

---

### 14.3 Identity & determinism

Sandbox identity is defined to make sandbox behavior reproducible across machines.

**Inputs to SD:**

* `base_os_oid` – identity of the sandbox base (filesystem tree + metadata).
* `capabilities_set` – set of sandbox capabilities (explicit + inferred).
* `profile_oid` – env profile id (runtime + packages + env vars).
* `sbx_version` – px sandbox layout/format version.

**Computed:**

```text
sbx_id := sha256( canonical_encode(
  "sandbox",
  {
    "base_os_oid": "<...>",
    "capabilities": sorted(list(capabilities_set)),
    "profile_oid": "<...>",
    "sbx_version": "<...>"
  }
))
```

Properties:

* For fixed px version, same `(base, capabilities, profile)` ⇒ same `sbx_id`.
* Changing any of:

  * base OS version,
  * capabilities,
  * env profile,
  * sandbox format

  yields a different `sbx_id`.

**Sandbox image identity**

* Each SI is tagged with `sbx_id` and an OCI digest.
* `sbx_id` is authoritative; digest is a concrete encoding for a particular registry/backend.

**Sandbox app bundles (`.pxapp`)**

* `sbx_id` continues to describe only the **sandbox** (base + capabilities + env); it is **independent** of project code.

* Pack operations may therefore produce multiple `.pxapp` bundles that share the same `sbx_id` but differ in app code snapshots.

* For a fixed `sbx_id` and a fixed app code snapshot, the contents of a `.pxapp` bundle are required to be deterministic (subject to normal reproducible‑build caveats such as timestamps).

* Any file‑level checksum or signature of the `.pxapp` itself is an implementation detail; the sandbox model’s conceptual identity remains:

  ```text
  ( SD / sbx_id , app_code_snapshot )
  ```

* `.pxapp` bundles do **not** introduce a new identity namespace or new CAS object kinds; they are deterministic exports of sandbox images plus app code.

---

### 14.4 Sandbox bases (SB)

px ships a curated set of **sandbox bases**:

* Each base is:

  * A minimal root filesystem with:

    * core libs (glibc/musl, libstdc++, libgcc),
    * basic utilities,
    * SSL/crypto libs,
    * a small set of extra packages common to Python.
  * Versioned and pinned (e.g. `px-base-3`).

* Each base is stored as a CAS object:

  ```text
  kind: "sandbox-base"
  payload:
    {
      "name": "debian-12",
      "variant": "py311",
      "px_base_version": 3,
      "os_release": { ... },
      "packages": [...],
      "fs_tree": <normalized tree spec>,
    }
  ```

* Sandbox bases are identified by:

  * Logical name in config: `debian-12`, `alpine-3.20`, etc.
  * Resolved `base_os_oid` at runtime by px.

* Default base:

  * If `base` is not set, px uses `debian-12` everywhere (including macOS hosts). Other bases (e.g. `alpine-3.20`) are opt-in.

**Non‑goal:** supporting arbitrary user‑constructed bases in v1.

* Advanced configs may allow overriding the base with a custom image reference, but this is an escape‑hatch, not the primary flow.

---

### 14.5 Capabilities (SC)

Capabilities abstract system‑level requirements away from distro package names.

* Each capability is:

  * A **name** (string, lower‑case, PEP 503‑style).
  * A mapping to OS packages per supported base.

Example (conceptual):

```text
capability "postgres":
  debian-12:
    packages: ["libpq5", "libpq-dev", "ca-certificates"]
  alpine-3.20:
    packages: ["libpq", "postgresql-dev", "ca-certificates"]

capability "imagecodecs":
  debian-12:
    packages: ["libjpeg62-turbo", "zlib1g", "libpng16-16"]
```

Capabilities are:

* Enumerated and curated by px.
* Intended to cover common Python system dependencies (db drivers, image processing, XML, LDAP, HTTP, etc.).
* Extensible in future versions; capability → packages mapping is part of sandbox format.

**Non‑goal:** arbitrary OS package management; px does not expose apt/dnf/apk directly.

px does not expose OS package managers or conda environments directly. Users
express system requirements only via capabilities; px translates those
capabilities into concrete OS packages inside sandbox images and builder
environments. The choice of package provider (apt/apk/dnf/conda-forge/...) is an
implementation detail and may change between px versions without affecting the
sandbox model.

---

### 14.6 Sandbox manifest (SM) & configuration

Per project/workspace, `SM` is derived from `pyproject.toml`:

```toml
[tool.px.sandbox]
base = "debian-12"          # default base; optional, px chooses if absent
auto = true                 # allow px to infer capabilities

[tool.px.sandbox.capabilities]
postgres    = true          # explicitly enabled
xml         = false         # explicitly disabled (overrides inference)
```

Rules:

* `base`:

  * Optional – px picks a default (e.g. `debian-12`) if unspecified.
  * Must correspond to a known sandbox base.

* `auto`:

  * Default `true`.
  * When `true`, px may infer capabilities from lock/ CAS / errors.
  * When `false`, px uses only explicitly declared capabilities.

* Workspace precedence:

  * Standalone project (no workspace): use `[tool.px.sandbox]` in that project’s `pyproject.toml`.
  * Workspace-governed project: only the workspace root `[tool.px.sandbox]` is read; member-level `[tool.px.sandbox]` is ignored.
  * `px status` should warn when a member defines `[tool.px.sandbox]` but a workspace root exists: workspace sandbox config is authoritative.

* Explicit `capabilities`:

  * `name = true` → capability is enabled regardless of inference.
  * `name = false` → capability is disabled, even if inference suggests it.

The **effective capabilities set** is:

```text
capabilities_effective :=
  explicit_true
  ∪ (auto_enabled_inferred_capabilities − explicit_false)
```

---

### 14.7 Capability inference

Inference is best‑effort and may be disabled (`auto = false`). It must be deterministic for a given px version, base, and profile.

Inference is **pure**: it affects only the sandbox build; px never writes inferred capabilities back to `pyproject.toml` or other project/workspace artifacts.

Inference has three layers, applied in order:

#### 14.7.1 Lock‑based mapping

Given a lockfile (L/WL) and `profile_oid`:

* For each package node with name `dist_name`:

  * If `dist_name` matches a **static mapping** (e.g. `psycopg2` → `postgres`, `Pillow` → `imagecodecs`), add those capabilities to the candidate set.

Mappings live in px code and are versioned with `sbx_version`.

#### 14.7.2 CAS `.so` inspection

Given a profile and its `pkg-build` objects:

* For each built extension `.so` in CAS:

  1. Inspect its dynamic library deps (`DT_NEEDED` ELF entries) without executing code.

  2. Match library names against known patterns:

     * `libpq.so.*` → `postgres`
     * `libjpeg.so.*` → `imagecodecs`
     * `libxml2.so.*` → `xml`
     * etc.

  3. Add corresponding capabilities.

This pass runs:

* Once per `pkg-build` object, cached by `pkg-build` oid.
* Inside the sandbox base OS environment or a neutral analyzer env, but never uses the host OS.

#### 14.7.3 Error‑driven inference

When `px run --sandbox`, `px test --sandbox`, or `px pack image` fails with known patterns:

* Build errors:

  * Missing headers `libpq-fe.h`, `jpeglib.h`, `zlib.h`, etc.
  * Missing build tools for known stacks.

* Runtime errors:

  * ELF loader messages: `error while loading shared libraries: libpq.so.5: cannot open shared object file: No such file or directory`.

px maps these patterns to capabilities and may:

* Offer a fix suggestion:

  ```text
  PX9xx  Missing Postgres client libraries in sandbox base.

  Fix:
    • Run: px sandbox add postgres
    • Or set [tool.px.sandbox.capabilities].postgres = true
  ```

* Optionally (controlled by a flag) auto‑add the capability when `auto = true`.

Inference is **advisory**; explicit config always wins.

Frozen/CI (`--frozen` / `CI=1`) behavior:

* Sandbox commands may still build/reuse images from a clean lock/env and inferred capabilities.
* If a required capability is missing, commands fail with an explicit suggestion; px never edits manifests or auto-adds capabilities in frozen/CI.

---

### 14.8 Sandbox lifecycle & storage

Sandbox images are treated as **deriveable artifacts** like envs.

Structure under a sandbox store root (e.g. `~/.px/sandbox/`):

```text
~/.px/sandbox/
  bases/
    <base_os_oid>/          # unpacked base FS; implementation detail
      manifest.json
      rootfs/...
  images/
    <sbx_id>/
      manifest.json         # references base_os_oid, profile_oid, capabilities, image digest(s)
      oci/                  # optional local OCI layout or tarballs
  tmp/
    ...                     # in-progress builds
```

* `manifest.json` for an SI:

  ```json
  {
    "sbx_id": "...",
    "base_os_oid": "...",
    "profile_oid": "...",
    "capabilities": ["postgres", "imagecodecs"],
    "image_digest": "sha256:...",
    "created_at": "...",
    "px_version": "...",
    "sbx_version": 1
  }
  ```

Sandbox store invariants:

* SI is immutable once created.
* SI can always be recomputed from `SB + profile + capabilities`.
* Garbage collection is safe once references (e.g. images in use, recent builds) are tracked; GC is an implementation detail, but must never remove bases or images referenced by manifests.

**`.pxapp` bundles and the sandbox store**

* `.pxapp` files are **user‑owned artifacts** produced by `px pack app` into arbitrary paths (e.g. `./dist/myapp.pxapp`); they are not required to live under `~/.px/sandbox/`.

* They are constructed from existing SI state and an app code snapshot:

  ```text
  SI (sbx_id) + app_code_snapshot → .pxapp
  ```

* Their existence does not change sandbox store invariants:

  * No new directories or CAS object kinds are required for `.pxapp`.
  * Running a `.pxapp` may cause px to reconstruct or cache an SI under `images/<sbx_id>` or `tmp/`, but this is a backend detail and must not alter project/workspace state.

---

### 14.9 Sandbox execution semantics (`px run --sandbox`, `px test --sandbox`)

Sandbox execution wraps the existing env execution model.

**Inputs:**

* Project or workspace context (as for `px run` / `px test`).
* Active env profile (`profile_oid`).
* Sandbox manifest (SM) → `base_os_oid` + `capabilities_effective`.

**Behavior (`px run --sandbox <target>`)**

1. **Resolve env profile**:

   * As in `px run`: ensure env exists and is clean (dev may rebuild env; CI/`--frozen` never resolves).

2. **Compute SD**:

   * Resolve `base` → `base_os_oid`.
   * Compute `capabilities_effective`.
   * Compute `sbx_id`.

3. **Ensure SI**:

   * If `images/<sbx_id>` exists and passes integrity checks, reuse.
   * Else:

     * Materialize SI:

       * Combine SB filesystem with runtime + env site‑packages:

         * Copy or mount the px runtime tree.
         * Copy or mount env site‑packages and scripts.
         * **Run/test:** bind‑mount the working tree into the container (e.g. at `/app`) for a tight dev loop; no image rebuild needed for code edits.
         * **Pack:** when invoked via packing commands, copy the working tree snapshot into the deployable artifact (see 14.10).
       * Install OS packages for `capabilities_effective` into the base via an internal mechanism (not exposed to users).
       * Write `manifest.json`.
     * Optionally push to/from a registry (implementation detail of `px pack image`).

4. **Run target inside SI**:

   * Start a container from SI:

     * Working tree is mounted (read‑only or read‑write; configuration dependent).
     * `PWD` is the project root inside container.
     * Env vars are set from profile/env manifest plus minimal host passthrough (HOME, TERM, etc.).

   * Use same target resolution as `px run` inside the container (scripts, `python`, files).

   * Frozen/CI: if a required capability is missing, fail with a suggestion; no auto-add or manifest edits.

5. **Exit semantics**:

   * Exit code is the exit code of the process inside the container.
   * No project/workspace state is mutated (no lock or env writes).

`px test --sandbox` behaves analogously but runs test target(s) inside SI.

**Reader vs mutator:**

* `px run --sandbox` / `px test --sandbox` are **reader commands** for project/workspace state, like `px fmt` (except they may materialize SI).

---

### 14.9.1 Executing sandbox app bundles (`px run <bundle.pxapp>`)

`px run` is extended with a mode that executes portable sandbox app bundles directly.

**Inputs & CLI shape:**

* A filesystem path whose basename ends in `.pxapp`:

  ```bash
  px run ./dist/myapp.pxapp [--] [args...]
  ```

* All arguments following the `.pxapp` path (optionally after a `--` separator) are forwarded to the bundle’s entrypoint without interpretation by px.

**Resolution rules:**

* If the first non‑option argument to `px run` is a path that exists and ends with `.pxapp`, px treats the command as **bundle execution**, regardless of whether the current directory is a project/workspace.
* In this mode, px:

  * does **not** read `pyproject.toml`, lockfiles, or env manifests from the current directory,
  * does **not** resolve envs from M/L/E or WM/WL/WE,
  * derives all runtime behavior from the contents of the `.pxapp` bundle.

**Behavior:**

1. **Open and validate bundle**

   * Confirm that the file is a valid `.pxapp`.
   * Read embedded metadata, including:

     * `sbx_id`, `base_os_oid`, `profile_oid`, `capabilities`,
     * app code snapshot root,
     * entrypoint specification (command and working directory),
     * px/sbx format versions.

2. **Ensure sandbox image for execution**

   * If an SI for `sbx_id` is already present in `images/<sbx_id>`, px may reuse it.
   * Otherwise, px reconstructs the SI from the bundle’s encoded layers into an internal location (e.g. under `tmp/` or `images/<sbx_id>`).
   * This reconstruction is **purely** a function of the bundle; it never consults project/workspace configuration.

3. **Launch containerized process**

   * px starts a container using the SI and the bundled app filesystem:

     * The app code visible inside the container is the snapshot embedded in the `.pxapp`, not the caller’s working tree.
     * `PWD` inside the container is the app root defined at pack time.
     * Environment variables are taken from the embedded profile (with minimal, controlled host passthrough such as `HOME`, `TERM` when appropriate).

   * The bundle’s configured entrypoint is invoked, and all CLI arguments after the `.pxapp` path are passed through verbatim.

   * Execution **always** uses a container backend; there is no host‑mode execution of `.pxapp` bundles.

4. **Exit semantics & mutability**

   * The exit code of `px run <bundle.pxapp>` is the exit code of the process inside the container.

   * From px’s perspective, `.pxapp` execution is **read‑only**:

     * no lockfiles are created or modified,
     * no envs are created, updated, or deleted,
     * project/workspace state machines are neither read nor mutated.

   * The containerized process may write to its own ephemeral filesystem or to volumes configured by the backend, but those writes are outside the px state model.

`px test` is not extended to accept `.pxapp` bundles in this iteration; bundles are treated as apps, not test harnesses.

---

### 14.10 Pack semantics (`px pack image`, `px pack app`)

Packing freezes an SD and a snapshot of the app code into a deployable artifact.

Two artifact forms are supported:

* **Container/OCI app images** – via `px pack image`.
* **Portable app bundles** – via `px pack app`, producing `.pxapp` files.

Both reuse the same sandbox definition, sandbox image pipeline, and app code snapshot behavior.

#### 14.10.1 Pack image (`px pack image`)

`px pack image` freezes an SD into a named, deployable OCI image.

**Inputs:**

* Project/workspace context.
* `profile_oid`.
* SD (`base_os_oid`, `capabilities_effective`, `sbx_version`).
* Optional user flags (`--tag`, `--out`, `--push`, `--allow-dirty`).

**Behavior:**

1. Compute `sbx_id` as in 14.3.

2. Ensure SI exists for `sbx_id` (as in 14.9’s “Ensure SI”). The SI contains base + system deps + Python runtime + env, but not project code.

3. **App code snapshot:**

   * Copy the working tree into an app filesystem tree inside the image (respecting `.gitignore` / standard Python packaging ignores).
   * Default: require a **clean working tree**; fail with a clear message if dirty.
   * Allow an explicit escape hatch (`--allow-dirty`) to pack with uncommitted changes.

4. **Naming:**

   * Derive a default image name if none provided (e.g. `px.local/<project_name>:<version-or-git-sha>`).
   * Allow explicit `--tag` to override.

5. **Encoding:**

   * Produce a valid OCI image:

     * `config` and `manifest` referencing layers derived from SB + env (from the SI),
     * one or more layers containing the app code snapshot and any pack‑time metadata.

6. **Output modes:**

   * `--out <path>` – write an OCI tarball or directory layout.
   * `--push` – push to a container registry (implementation detail); failures here are reported as pack errors.
   * Without flags, px may store the image locally and print:

     ```text
     Built image:
       name: ghcr.io/org/myapp:1.2.3
       digest: sha256:...

     Derived from:
       base: debian-12 (base_os_oid=...)
       env:  profile_oid=...
       sbx:  sbx_id=...
     ```

7. **Mutability:**

   * `px pack image` **never** mutates project/workspace manifests or locks.
   * SI and registry state are mutable; GC and retention policies are implementation details.

#### 14.10.2 Pack app (`px pack app`) – portable `.pxapp` bundles

`px pack app` produces a **single‑file portable bundle** encoding the same sandboxed app as `px pack image`, but in a `.pxapp` format optimized for distribution and `px run`.

**Inputs:**

* Project/workspace context.
* `profile_oid`.
* SD (`base_os_oid`, `capabilities_effective`, `sbx_version`).
* Optional user flags:

  * `--out <path>` – output `.pxapp` path (e.g. `./dist/<project>-<version>.pxapp` by default).
  * `--allow-dirty` – permit packing from a dirty working tree, same semantics as 14.10.1.
  * Optional entrypoint overrides (e.g. `--entrypoint`), if supported.

**Behavior:**

1. Compute `sbx_id` and ensure SI exactly as in 14.10.1 steps 1–2. The same SI is reused for image and bundle packaging.

2. **App code snapshot:**

   * Snapshot the working tree using the **same rules and cleanliness checks** as `px pack image` (14.10.1 step 3).
   * For a given project state, `px pack image` and `px pack app` see the same code snapshot.

3. **Bundle construction:**

   * Serialize the sandboxed app into a single `.pxapp` file that contains:

     * an encoding of the SI layers (base + system deps + runtime + env) sufficient to reconstruct the sandbox image,
     * the app code snapshot under a well‑defined root path,
     * minimal runtime metadata:

       * `sbx_id`, `base_os_oid`, `profile_oid`, `capabilities`,
       * entrypoint command and working directory,
       * px and sandbox format versions,
       * optional descriptive metadata (app name, version, build timestamp, provenance, signatures, etc.).

   * The precise file format is opaque; the spec only guarantees that `.pxapp` is a **portable, single‑file encoding** of a sandbox image plus app snapshot.

4. **Output:**

   * Write the `.pxapp` file to the specified `--out` path, or to a reasonable default if unspecified.
   * No registry push is performed; `.pxapp` is a filesystem artifact.

5. **Determinism:**

   * For a fixed SD and identical app code snapshot, repeated `px pack app` must produce deterministic `.pxapp` contents (to the same degree that `px pack image` is deterministic).
   * `.pxapp` determinism is inherited from:

     * `sbx_id` (sandbox identity),
     * the deterministic working tree snapshot,
     * the deterministic bundle encoding.

6. **Mutability:**

   * `px pack app` **never** mutates project/workspace manifests or locks.
   * It may create or reuse SI entries under `~/.px/sandbox/images/`, but `.pxapp` files themselves live wherever the user chooses.

**Relationship between `px pack image` and `px pack app`:**

* Both commands package the same conceptual **sandboxed app**:

  ```text
  sandboxed_app := (SD, app_code_snapshot, entrypoint, metadata)
  ```

* `px pack image` encodes `sandboxed_app` as an OCI image.

* `px pack app` encodes `sandboxed_app` as a `.pxapp` file.

* Neither introduces new sandbox identities or CAS object types; they are alternate encodings of the same underlying SD and app snapshot.

---

### 14.11 Integration with project & workspace state machines

Sandbox is layered strictly **on top** of existing state machines:

**Commands that may touch sandbox:**

* `px run --sandbox` / `px test --sandbox` – reader commands w.r.t M/L/E and WM/WL/WE.
* `px pack image` / `px pack app` – pure consumers; never modify M/L/E or WM/WL/WE.
* `px run <bundle.pxapp>` – reader command that does **not** depend on project/workspace state at all; it executes a self‑contained bundle.

**Dependencies:**

* `px run --sandbox` and `px test --sandbox` require:

  * Project/workspace `manifest_exists == true`.
  * In dev: may accept `NeedsEnv` and rebuild env.
  * In frozen/CI: require `Consistent` / `WConsistent`, as for non‑sandbox variants.

* `px pack image` and `px pack app` require:

  * `env_clean == true` / `w_env_clean == true` (consistent env).
  * A clean working tree by default; they fail if the working tree is dirty unless `--allow-dirty` is passed.
  * They fail if env is missing/stale; suggest `px sync`.

* `px run <bundle.pxapp>` requires:

  * Only that the path refers to a valid `.pxapp` file and a compatible container backend.
  * It does **not** require `manifest_exists`, `NeedsEnv`, or any workspace invariants.
  * Even when invoked inside a project, it bypasses project/workspace state machines entirely; all necessary information comes from the bundle.

**Reader vs mutator:**

* `px run --sandbox`, `px test --sandbox`, `px pack image`, and `px pack app` are **reader/consumer** commands with respect to M/L/E and WM/WL/WE.
* `px run <bundle.pxapp>` is an even stricter reader:

  * It never inspects or mutates M/L/E or WM/WL/WE.
  * It is safe to run in arbitrary directories (including non‑project locations such as `/tmp` or a user’s home directory).

Introducing `.pxapp` therefore does not add new state‑machine surfaces; it reuses the existing sandbox identity and env/profile machinery.

---

### 14.12 Error model & observability

Sandbox introduces a dedicated error family, e.g.:

* `PX900` – sandbox base unavailable or incompatible.

  * **Why**:

    * Requested sandbox base `debian-12` is unknown or incompatible with this platform/px version.

  * **Fix**:

    * `• Remove or change [tool.px.sandbox].base.`
    * `• Upgrade px to a version that supports this base.`

* `PX901` – sandbox capability resolution failure.

  * **Why**:

    * px cannot satisfy capability `postgres` on base `alpine-3.20`.

  * **Fix**:

    * `• Change [tool.px.sandbox].base to a supported base.`
    * `• Or remove/disable that capability.`

* `PX902` – sandbox system dependency missing.

  * **Why**:

    * A system library needed by your env (e.g. `libpq.so.5`) is missing from the sandbox image.

  * **Fix**:

    * `• Run 'px sandbox add postgres' or set [tool.px.sandbox.capabilities].postgres = true.`
    * `• Re-run 'px run --sandbox', 'px pack image', or 'px pack app'.`
    * `• If running a .pxapp, verify the bundle was built with the needed capability and recreate if not.`

* `PX903` – sandbox build failed.

  * **Why**:

    * Underlying image build/backend failed (disk full, permissions, registry error, bundle reconstruction error).

  * **Fix**:

    * `• Check disk space and registry credentials.`
    * `• Retry 'px pack image', 'px pack app', or 'px run --sandbox'.`
    * `• If running a .pxapp, verify the bundle is not corrupted and was built with a compatible px version.`

* `PX904` – sandbox format mismatch.

  * **Why**:

    * Sandbox image or `.pxapp` bundle was built with incompatible `sbx_version` or px version.

  * **Fix**:

    * `• Rebuild sandbox images and bundles with this px version.`
    * `• Or clear the sandbox store and rerun.`

Observability:

* px logs:

  * sandbox inference decisions (which capabilities were added and why),
  * base choice,
  * image build cache hits/misses,
  * pack actions (`px pack image`, `px pack app`) including app snapshot roots and bundle/image outputs,
  * `.pxapp` execution details (which `sbx_id` and entrypoint were used).

* Optional command `px sandbox explain`:

  * Prints the effective sandbox configuration (base, capabilities, sources of each capability: static map, CAS, explicit).
  * May include, when relevant, information about which `sbx_id` and capabilities a given `.pxapp` depends on.

---

### 14.13 Packaging & portability: images vs `.pxapp` bundles

Sandbox supports two complementary distribution mechanisms for sandboxed apps:

* **Registry‑oriented packaging – `px pack image`**

  * Produces OCI images suitable for container registries and orchestrators.
  * Best for deployment pipelines that already integrate with Docker/OCI tooling.

* **File‑oriented packaging – `px pack app` → `.pxapp`**

  * Produces a single‑file portable bundle containing:

    * the sandbox base and system libraries,
    * the Python runtime and env,
    * a snapshot of the project code,
    * an entrypoint and minimal metadata.

  * Optimized for:

    * ad‑hoc sharing (e.g. sending an app file to a colleague),
    * running on machines that have `px` and a container backend but no registry access,
    * reproducible demos and examples (`px run ./example.pxapp`).

From the SBX model’s point of view:

* Both forms are **exports** of the same underlying sandbox definition and env profile:

  ```text
  SD → SI → { OCI app image, .pxapp }
  ```

* They do **not** introduce new sandbox identities or CAS object types.

* They inherit determinism from:

  * the sandbox identity (`sbx_id`),
  * the deterministic env/materialization pipeline,
  * the deterministic app code snapshotting behavior.

`.pxapp` bundles may embed additional metadata (names, descriptions, build provenance, signatures, etc.), but the conceptual identity of the sandbox remains the sandbox definition and env profile; `.pxapp` is purely a transport and execution format built on top of SBX.
