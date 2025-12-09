# Determinism and CI

px behavior is built around two parallel state machines (project and workspace) with explicit transitions. Determinism and explicit mutation are enforced so that the same inputs produce the same outputs locally and in CI.

## Principles

* **Two parallel state machines, same shape** – each project and workspace is described by Manifest/Lock/Env artifacts and a small state machine; commands are transitions over these machines. If a workspace governs a project, the workspace machine is authoritative for deps/env.
* **Determinism** – given the same project/workspace, runtimes, and configuration, px must make the same decisions: same runtime, same lockfile(s), same env(s), same command resolution.
* **Smooth UX, explicit mutation** – mutating operations are explicit (`init`, `add`, `remove`, `sync`, `update`, workspace sync/update, `tool install/upgrade/remove`). Reader commands (`run`, `test`, `fmt`, `status`, `why`) never change manifests or locks and have tightly bounded behavior when they repair envs.

## Deterministic surfaces

For a fixed px version, runtime set, platform, and index configuration, the following must be deterministic:

1. **Runtime selection**

   * Project runtime resolution follows a fixed precedence.
   * Tool runtime resolution follows a fixed precedence.
   * No guessing a different runtime across runs for the same inputs.

2. **Lockfile generation**

   * Given manifest, runtime, platform, and index configuration, the resolver must produce the same `px.lock` (including ordering, `mfingerprint`, and lock ID).

3. **Environment materialization**

   * Given a lock and runtime, the environment must contain exactly the packages described by that lock.
   * Rebuilding for the same lock must result in an equivalent environment (from px’s metadata point of view).

4. **Target resolution for `px run`**

   * A fixed, documented rule; no hidden fallbacks like `<package>.cli`.

5. **Non-TTY output and `--json`**

   * No interactive spinners or frame-based progress when stderr is non-TTY or `--json` is set.
   * Output is line-oriented or structured JSON with stable shapes and ordering.

6. **Error codes and shapes**

   * Stable PX error codes and “Why/Fix” structure; wording may improve but semantics remain.

7. **Workspace lockfile generation**

   * Given workspace manifest, runtime, platform, and index configuration, the resolver must produce the same `px.workspace.lock` (including ordering, `wmfingerprint`, and workspace lock ID).

8. **Workspace environment materialization**

   * Given `px.workspace.lock` and runtime, the workspace environment must contain exactly the packages described by WL.
   * Rebuilding for the same WL must result in an equivalent environment (from px’s metadata point of view).

9. **Native builds via builders**

   * For a fixed px version, builder set (`builder_id`), runtime, platform, and index configuration, building from the same `source_oid` and build options must produce the same `pkg-build` CAS object.
   * Builders are versioned; changing the underlying builder image (OS, toolchain, OS package provider) must bump `builder_id` so new builds use a new `build_key` while existing CAS objects remain valid.

## CI and frozen mode

* Under `CI=1` or `--frozen`, px never re-resolves locks.
* `px run` / `px test` / `px fmt` do not rebuild project/workspace envs; they check consistency and fail if broken (for run/test) or run tools in isolation (`fmt`).
* No prompts or implicit mutations. Output must be non-interactive; follow the non-TTY rules above.

Native builds in CI always run inside the same px-managed builders as on developer machines; px never uses ad-hoc host compilers or user-managed conda envs for producing `pkg-build` artifacts.

These rules keep local development and CI reproducible and make state drift visible instead of silently patched.

## Commit-scoped environments

* `px run --at <git-ref>` / `px test --at <git-ref>` execute using the manifest + lock at that ref without checking it out.
* Env/profile identity is a function of the git ref, manifest/lock content, and runtime; environments are materialized/reused from the global CAS cache and never touch the working tree.
* Frozen semantics: if the ref lacks a lock or it drifts from the manifest fingerprint at that ref, the command fails instead of re-resolving.
