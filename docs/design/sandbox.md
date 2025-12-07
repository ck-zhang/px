## 14. Sandbox (SBX)

### 14.1 Goal & scope

Sandboxing adds a second deterministic layer on top of px envs:

* **Env layer (existing)** – runtime + Python packages + px env layout (CAS profile).
* **Sandbox layer (new)** – base OS + system libraries + containerized execution.

The sandbox layer exists to support:

* Packages and apps that depend on **system libraries** (`libpq`, `libjpeg`, `libxml2`, etc.).
* **Containerized execution** (dev & CI) that matches production images.
* Deterministic, repeatable **deployable images** (`px pack image`) without exposing Docker complexity.

Sandbox is **optional**:

* Default workflow uses host OS + px envs (`px run`, `px test`).
* Sandbox flows are opt‑in (`px run --sandbox`, `px test --sandbox`, `px pack image`).

Sandbox is **derived** from the env profile and a small **sandbox manifest**; it does not mutate project/workspace state.

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

  * Fully determined by `sbx_id`.
  * Used ephemerally for `px run --sandbox` / `px test --sandbox`, and persistently for `px pack image`.

Sandbox is **orthogonal** to:

* CAS (objects, profiles, env materialization).
* Project/workspace state machines (M/L/E and WM/WL/WE).

Sandbox consumes `profile_oid` as an input; it never changes locks or envs.

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
         * **Pack:** copy the working tree into the image (see 14.10).
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

### 14.10 Pack semantics (`px pack image`)

`px pack image` freezes SD into a named, deployable image.

**Inputs:**

* Project/workspace context.
* `profile_oid`.
* SD (`base_os_oid`, `capabilities_effective`, `sbx_version`).
* Optional user flags (`--tag`, `--out`, `--push`).

**Behavior:**

1. Compute `sbx_id` as in 14.3.

2. Ensure SI exists for `sbx_id` (as in 14.9’s “Ensure SI”).

3. **App code:**

   * Copy the working tree into the image (respecting `.gitignore` / standard Python packaging ignores).
   * Default: require a clean working tree; fail with a clear message if dirty.
   * Allow an explicit escape hatch (`--allow-dirty`) to pack with uncommitted changes.

4. **Naming:**

   * Derive a default image name if none provided (e.g. `px.local/<project_name>:<version-or-git-sha>`).
   * Allow explicit `--tag` to override.

5. **Encoding:**

   * Produce a valid OCI image:

     * `config` and `manifest` referencing the layers derived from SB and env profile.
     * Layers are content‑addressed and deduplicated where possible.

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

**Mutability:**

* `px pack image` **never** mutates project/workspace manifests or locks.
* SI and registry state are mutable; GC and retention policies are implementation details.

---

### 14.11 Integration with project & workspace state machines

Sandbox is layered strictly **on top** of existing state machines:

* Commands that may touch sandbox:

  * `px run --sandbox` / `px test --sandbox` – reader commands w.r.t M/L/E and WM/WL/WE.
  * `px pack image` – pure consumer; never modifies M/L/E or WM/WL/WE.

* Dependencies:

  * `px run --sandbox` and `px test --sandbox` require:

    * Project/workspace `manifest_exists == true`.
    * In dev: may accept `NeedsEnv` and rebuild env.
    * In frozen/CI: require `Consistent` / `WConsistent`, as for non‑sandbox variants.

  * `px pack image` requires:

    * `env_clean == true` / `w_env_clean == true` (consistent env).
    * Fails if env is missing/stale; suggests `px sync`.

* Sandbox manifest (SM):

  * Reads `[tool.px.sandbox]` from `pyproject.toml` at project or workspace root.
  * Does not add new per‑project or per‑workspace artifacts beyond what’s already allowed (no new top‑level files).

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
    * `• Re-run 'px run --sandbox' or 'px pack image'.`

* `PX903` – sandbox build failed.

  * **Why**:

    * Underlying image build/backend failed (disk full, permissions, registry error).

  * **Fix**:

    * `• Check disk space and registry credentials.`
    * `• Retry 'px pack image' or 'px run --sandbox'.`

* `PX904` – sandbox format mismatch.

  * **Why**:

    * Sandbox image was built with incompatible `sbx_version` or px version.

  * **Fix**:

    * `• Rebuild sandbox images with this px version.`
    * `• Or clear the sandbox store and rerun.`

Observability:

* px logs:

  * sandbox inference decisions (which capabilities were added and why),
  * base choice,
  * image build cache hits/misses.

* Optional command `px sandbox explain`:

  * Prints the effective sandbox configuration (base, capabilities, sources of each capability: static map, CAS, explicit).
