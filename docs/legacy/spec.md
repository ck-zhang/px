<!-- markdownlint-disable MD013 MD040 -->

# px – Opinionated Python Toolchain (Rust)

## 0) Mission

One binary that makes Python development **canonical**: `init → add → run → test → fmt → build → publish`.
**No venv activation**, **deterministic installs**, **fast from cache**.
**UX feels like Go**, **model behaves like Cargo**.

---

## 1) Core Principles

* **One obvious way:** enforced layout, minimal config.
* **No activation ceremony:** px prepares + injects env; users never touch it.
* **Reproducibility first:** mandatory lockfile; `--frozen` installs.
* **Speed via Rust:** parallel I/O, global CAS, incremental everything.
* **Security baseline:** TLS + hash pinning; provenance later.
* **Terse, actionable UX:** blunt errors with explicit next steps.

---

## 2) Storage & Environment Model (UPDATED)

### Global Store (CAS)

```
~/.cache/px/store/<algo>/<content-hash>/
  dist/               # extracted wheel contents (directory importable by CPython)
  meta.json           # {name, version, tags, py, abi, sha256, build}
  wheel.whl           # optional: original artifact (not used on sys.path when native)
```

* **Key:** content hash of the wheel file (sha256 of canonical wheel bytes).
* Always keep an **extracted** importable directory (needed for native extensions).

### Per‑Project View (hidden)

```
project/
  .px/
    site/px.pth      # absolute paths into the global store, one per line
    bin/             # console-script shims
    env.json         # selected interpreter, target tags, resolved dists (not committed)
  px.lock            # committed, authoritative
  pyproject.toml
  src/<package>/
  tests/
```

**Bootstrap/Runner:**

* `px run` executes the selected Python with:

  * `-s` (disable user site), `PYTHONNOUSERSITE=1`
  * a short bootstrap that runs `site.addsitedir("./.px/site")` so `px.pth` expands to store paths
  * `src/` added before third‑party paths
* **No reliance on `__pypackages__`.**
* **No venv** required; optional venv‑compat view exists (below).

**Optional compatibility modes (not default):**

* `px env --mode pep582` → materialize `__pypackages__` for legacy tools.
* `px env --mode venv` → create `.px/venv/` with a `.pth` that points to store; for stubborn IDEs.

---

## 3) CLI (initial)

```
px init [--package <name>] [--py <X.Y>]
px add <spec>... [--dev] [--extra <name>]
px remove <name>...
px sync [--frozen] [--offline]
px update [<name>...] [--latest]
px run [<module|script|entry>] [-- <args...>]
px test [-- <pytest_args...>]
px fmt
px lint
px tidy [--apply] [--json]
px build [sdist|wheel|both] [--out <dir>]
px publish [--registry <name>] [--token-env <VAR>]

px cache [path|stats|prune]
px env [info|paths|python|materialize --mode {pep582|venv}]
```

* Default tools: `pytest`, `ruff` (formatter+lint) as managed dev‑deps.

---

## 4) Project Model

* **Enforced layout:** `src/<package>/`, `tests/`, `pyproject.toml`, `px.lock`, hidden `.px/`.
* **Scripts:** `[tool.px.scripts] name = "pkg.module:func"` → `px run name`.
* **Optional deps (“features”):** via `[project.optional-dependencies]` (Cargo‑like features, Python extras under the hood).

---

## 5) Resolver & Lockfile

* Full PEP 440/508, markers/extras, platform tags; prefer wheels, fallback to sdists.
* Deterministic backtracking; clear conflict messages.
* **Lockfile (`px.lock`, TOML):** exact artifacts (`name`, `version`, `filename`, `sha256`, `tags`, parents).
* **Install rules:** Only `px update` changes versions; `px sync --frozen` enforces lock.

---

## 6) Build & Install

* **Build:** PEP 517 in isolated ephemeral env (only build‑requires). Cache built wheels per target.
* **Install:** Ensure artifact exists in global store (download/build), update `.px/site/px.pth`, regenerate shims in `.px/bin/`.
* **Local/VCS deps:** record path/commit in lock; store contains built artifact per commit.

---

## 7) Workspaces (Cargo‑like)

```toml
[tool.px.workspace]
members = ["libs/core", "apps/api", "apps/worker"]
```

* Single root `px.lock`.
* `px test`/`px build` operate across members; `-p <member>` to target one.
* Intra‑workspace deps resolved without publishing.

---

## 8) Registries

```toml
[tool.px.registry]
default = "pypi"

[tool.px.registries.corp]
simple = "https://pypi.corp/simple/"
upload = "https://pypi.corp/upload/"
```

* Auth via env/NETRC/keyring.
* Mirror pinning, offline mode supported.

---

## 9) Security

* Baseline: TLS, artifact SHA256 verification against lock.
* Policies: allow/deny, registry pinning, offline enforcement.
* Roadmap: TUF (PEP 458/480), Sigstore attestations, optional lock signing.

---

## 10) IDE & CI

* `px env python` → path to **project interpreter shim** that preloads `.px/site` (works in IDEs).
* VS Code/PyCharm snippets generated.
* CI: `px sync --frozen`; cache `px cache path`.

---

## 11) Observability & UX

* Quiet by default; `-v` shows phases (resolve/download/build/install).
* Error pattern: short cause + concrete fix.
* `--json` for machine consumption; telemetry off by default.

---

## 12) `px tidy` (safe)

* Dry‑run by default; `--apply` to mutate.
* Removes unused **direct** deps from `pyproject`, drops orphaned transitives from lock, normalizes specs, cleans stale `.px` artifacts.
* **Never** upgrades versions or touches global store unless asked.

---

## 13) Interop (bridges, not clones)

* Import: `px import requirements.txt|poetry|pdm`.
* Export: `px export requirements.txt --locked`.
* Optional env materialization (`pep582`/`venv`) for edge tools.

---

## 14) Phase Goals (no timelines)

### **Phase A — MVP (Solo Dev)**

* Global CAS store + per‑project `.px/site/px.pth` + bootstrap runner.
* Core CLI (`init/add/remove/install/run/test/fmt/lint/build/publish/cache/env/tidy`).
* Resolver (markers/extras), PyPI client, wheel build/install.
* `px.lock` (single‑target), Linux/macOS, basic Windows.

**Exit:** New app/library created, locked, reproduced on another machine with `--frozen`.

---

### **Phase B — Team & CI**

* Lock diffing; machine‑readable outputs.
* CI cache flows; offline install from cache.
* VCS/path deps with commit pinning; robust Windows shims.
* IDE shims: `px env python` works everywhere.

**Exit:** Multi‑dev project clone → `px sync --frozen` → `px test` reliably; high cache hits.

---

### **Phase C — Workspaces & Multi‑Target Lock**

* Workspaces with single root lock.
* Multi‑target entries (OS/arch/Python ABI) in `px.lock`.
* Smarter native builds (toolchain detection; per‑target wheel cache).
* Registry abstraction/mirrors hardened.

**Exit:** Monorepo builds/tests across OSes with one lock.

---

### Lock schema v2 (graph/targets/artifacts)

* `px.lock` version 2 keeps the Phase A `[[dependencies]]` table for backward
  compatibility, but adds explicit dependency graph data:
* `[[graph.nodes]]` capture `name`, `version`, `marker`, and `parents`
  (defaulting to `"root"` for direct requirements).
* `[[graph.targets]]` enumerate `(python_tag, abi_tag, platform_tag)`
  triples so a single lock can describe multiple interpreter/ABI/platform
  outputs.
* `[[graph.artifacts]]` link nodes to targets and record the same wheel
  metadata used in v1 (`filename`, `url`, `sha256`, `size`, `cached_path`).
* `px sync` continues to emit v1 by default until the full resolver/store
  understands multi-target graphs. `px lock upgrade` converts existing locks to
  version 2 by dual-writing the new graph tables alongside the legacy
  dependency list.
* Verification flows (`px lock diff`, `px sync --frozen`, workspace tidy
  modes) normalize either version into a comparable snapshot so mixed repos can
  migrate gradually without drift noise.

---

### **Phase D — Org Adoption**

* Managed Python (downloaded, pinned); interpreter recorded in lock metadata.
* Policy hooks, audits, SBOM, offline/air‑gapped prefetch.
* Optional env materialization for legacy deploy targets.

**Exit:** Orgs can standardize on px; hermetic CI; policy satisfied.

---

### **Phase E — GA Hardening**

* Transactional installs, rollback, cache integrity + recovery.
* Lock v2 (back‑compatible), optional signing.
* Migration guides; `px doctor`.
* Public conformance + performance suite.

**Exit:** Stable, widely adoptable 1.0‑quality toolchain.

---

## 15) Success Metrics

* Repro: `--frozen` zero‑drift across machines/OS.
* Speed: cold/warm installs vs pip/Poetry/uv.
* DX: clone → tests passing in ≤ 3 commands.
* CI: cache hit rate; reduction in dependency‑related flakes.
* Adoption: % projects running with **zero** custom config.

---

## 16) Open Decisions

* **Runner isolation:** default `-s` + `PYTHONNOUSERSITE` vs stronger bootstrap with `-S` (manual path setup).
* **Formatter:** `ruff format` only vs `ruff+black` (pick one; avoid config sprawl).
* **Lock portability:** single multi‑target lock vs per‑target files (default: multi‑target once Phase C lands).
* **Single‑file packaging:** first‑class or integrate external tools later.

---

### Appendix A — Example `px.pth`

```
/home/alice/.cache/px/store/sha256/ab12.../dist
/home/alice/.cache/px/store/sha256/cd34.../dist
/project/src
```

(*`px run` injects `.px/site` via bootstrap, then runs the target.*)

### Appendix B — Example Flow

```bash
px init --package acme_demo
px add fastapi uvicorn
px run -m acme_demo.app
px test
px fmt && px lint
px build both
px sync --frozen   # CI
px tidy               # plan
px tidy --apply       # apply
```

**Bottom line:** This design keeps the repo clean (no `__pypackages__`), achieves **cache‑backed speed**, and gives Python exactly what it needs via a **tiny hidden `.px/` view**—while preserving the ambition to become the **default** Python toolchain.
