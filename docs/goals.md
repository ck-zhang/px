# Goals & Status

## Product-level milestones

- [x] Deterministic CLI surface wired to `px-core`, covering init/add/remove/sync/update/run/test plus supporting verbs such as `status`, `fmt`, `lint`, `build`, and `migrate` (`crates/px-cli/src/main.rs`).
- [x] Resolver + lockfile pipeline that understands pinned dependency graphs and artifacts via `pep440_rs`/`pep508_rs` (`crates/px-resolver`, `crates/px-lockfile`).
- [x] Pyproject authoring that edits `[project]` metadata and dependency arrays directly (PEP 621 aligned) (`crates/px-project/src/manifest.rs`).
- [ ] Color-coded Python tracebacks with px-specific remediation hints surfaced under failures in `px-cli`.

## PEP compatibility tracker

- [x] **PEP 440** – Version specifiers enforced everywhere via `pep440_rs`.
- [x] **PEP 508** – Requirement parsing, normalization, and marker evaluation handled with `pep508_rs`.
- [ ] **PEP 685** – Resolver still lacks conflict messaging for incompatible direct URLs/extras; no dedicated handling detected.
- [ ] **PEP 241** – Legacy metadata 1.0 not yet emitted or consumed anywhere in the build path.
- [ ] **PEP 314** – Same as above for metadata 1.1.
- [ ] **PEP 345** – Same as above for metadata 1.2.
- [ ] **PEP 566** – Metadata 2.1 support still absent from builders/publishers.
- [ ] **PEP 643** – Dynamic metadata (including SPDX license expression) not surfaced in current manifests.
- [ ] **PEP 639** – License-expression enforcement pending; manifests currently treat `license` opaquely.
- [ ] **PEP 517** – Build backend isolation flow not wired; `px build` command still shells out through legacy paths.
- [ ] **PEP 518** – `build-system` requirements from `pyproject.toml` are not resolved/satisfied before builds.
- [x] **PEP 621** – `[project]` tables are created and edited directly by `px-project`.
- [ ] **PEP 660** – Editable installs are not exposed; resolver rejects URL requirements entirely.
- [ ] **PEP 735** – Optional-dependency group metadata not yet reconciled with lockfile graph.
- [ ] **PEP 376** – Installed-record (`RECORD`) management for px-managed environments not implemented.
- [ ] **PEP 610** – No generation of `direct_url.json` files for direct URL installs.
- [ ] **PEP 427** – Wheel build guarantees not yet enforced when running `px build`.
- [ ] **PEP 425** – Compatibility tag evaluation is stubbed (ResolverTags only track python/abi/platform strings).
- [ ] **PEP 513** – manylinux1 compatibility/tag constraints still missing.
- [ ] **PEP 571** – manylinux2010 untreated.
- [ ] **PEP 599** – manylinux2014 untreated.
- [ ] **PEP 600** – Future manylinux policy versioning untreated.
- [ ] **PEP 656** – musllinux tags untreated.
- [ ] **PEP 503** – Simple repository API consumption not wired; resolver currently uses only the JSON API.
- [ ] **PEP 629** – Normalized simple API responses (including new metadata sections) not yet consumed.
- [ ] **PEP 658** – Embedded metadata (via `data-dist-info` in simple API) unsupported.
- [ ] **PEP 714** – Signature/metadata fetch requirements for `simple` responses unimplemented.
- [ ] **PEP 691** – New JSON-simple API not yet adopted (current resolver still hits `/pypi/<name>/json`).
- [ ] **PEP 592** – Yanked release flags not honored when picking artifacts.
- [ ] **PEP 700** – Source distribution metadata from `sdist` archives not ingested.
- [ ] **PEP 708** – Supply-chain `requires-dist` provenance for lockfiles not implemented.
- [ ] **PEP 740** – Metadata-only wheels/sdists not recognized.
- [ ] **PEP 792** – No support for trusted publisher (PyPI OIDC) metadata flows yet.
- [ ] **PEP 405** – Intentionally not targeted; px uses its own deterministic `.px/envs` manager instead of stdlib `venv`.
- [ ] **PEP 668** – No detection/emission of `EXTERNALLY-MANAGED` markers inside px-managed envs.
- [ ] *(Optional)* **PEP 694** – Upload API work unstarted.
- [ ] *(Optional)* **PEP 807** – Warehouse upload V2 integration unstarted.

## Traceback UX & px recommendations

- [ ] Apply px-specific color theming to the default Python traceback to highlight file/line/module, error type, and message.
- [ ] Missing import → suggest `px add <pkg>`.
- [ ] Declared dependency not installed → suggest `px install`.
- [ ] Environment out of sync with lock → suggest `px install`.
- [ ] No px project found → suggest `px init`.
- [ ] Legacy project detected → suggest `px migrate`.
- [ ] Dependency version incompatible with Python → suggest `px env python <ver>` or a compatible version.
- [ ] Dependency version incompatible with constraints → suggest a relaxed spec or a compatible version.
- [ ] Wrong interpreter (system python) detected → suggest `px run ...`.
- [ ] Missing dev tool during command → suggest `px add --dev <tool>`.
- [ ] Missing build backend → suggest adding one (e.g., `hatchling`).
- [ ] Typos in command → suggest the closest matching px verb.
- [ ] ABI mismatch (e.g., numpy) → suggest reinstalling or bumping Python.
- [ ] Newer versions available → suggest `px update`.
- [ ] Attempt to run Python file outside a project → suggest `px init` or `px run --no-project`.
- [ ] Attempt to add invalid package spec → suggest corrected syntax.
- [ ] Missing script/run target → suggest available tasks from `[tool.px.scripts]`.
- [ ] Missing or unsupported Python version → suggest a valid interpreter target.
- [ ] `requirements.txt` present → suggest `px migrate`.
- [ ] Multiple conflicting dependency sources → suggest choosing one or using `px migrate --from`.
- [ ] User runs `px install` with no deps → show hint about `px add`.
