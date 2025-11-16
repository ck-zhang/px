# Phase Requirements Backlog

Source reference: `docs/spec.md` (notably §14 "Phase Goals" plus storage,
environment, and CLI sections). Each phase entry documents: intent summary,
acceptance criteria, exit check, prioritized backlog, and critical
dependencies.

## Phase A — MVP (Solo Dev)

- **Goal:** single binary lets a solo dev `px init → add → install →
  run/test/fmt/lint/build/publish/cache/env/tidy` on Linux/macOS (baseline
  Windows) with CAS-backed envs and deterministic locks.
- **Acceptance criteria:** global CAS + `.px/site/px.pth` bootstrap; resolver
  with markers/extras, PyPI client, wheel build/install; single-target
  `px.lock`; no manual venv activation.
- **Exit check:** create sample project, lock it, and reproduce elsewhere via
  `px install --frozen` followed by `px test`.
- **Prioritized backlog:** (1) CAS store + `.px` bootstrap runner + env
  injection; (2) resolver + PyPI client + wheel build/install backing the CLI;
  (3) single-target lock emit/consume plus `--frozen` enforcement and smoke
  tests across Linux/macOS/basic Windows.
- **Dependencies:** resolver/CLI (2) depends on store/bootstrap (1);
  lock enforcement (3) depends on resolver output.

## Phase B — Team & CI

- **Goal:** enable multi-dev repos and CI by making installs reproducible,
  debuggable, and cache-friendly across platforms.
- **Acceptance criteria:** lock diffing + `--json` outputs; CI cache flows +
  offline install; VCS/path deps pinned to commits; hardened Windows shims; IDE
  interpreter shim via `px env python`.
- **Exit check:** fresh clone → `px install --frozen` → `px test` hits caches
  in CI/IDE scenarios with diff tooling for reviews.
- **Prioritized backlog:** (1) lock diff + JSON plumbing + exit codes; (2)
  cache priming/offline install flows, Windows shim parity, registry auth
  reuse; (3) IDE shim + VCS/path dep pinning + docs for multi-dev setups.
- **Dependencies:** requires Phase A CAS/resolver/lock stability; caching needs
  deterministic artifact IDs.

## Phase C — Workspaces & Multi-Target Lock

- **Goal:** support monorepos with one lock spanning members, OS/arch/ABI
  targets, and smarter native builds.
- **Acceptance criteria:** `[tool.px.workspace]` with single root lock;
  multi-target lock entries; per-target wheel cache + toolchain detection;
  hardened registry/mirror abstraction.
- **Exit check:** workspace repo builds/tests across Linux/macOS/Windows (and
  relevant ABIs) using one lockfile.
- **Prioritized backlog:** (1) workspace metadata parsing + resolver support +
  root lock recording per-target tuples; (2) native build orchestration with
  toolchain detection + per-target caches; (3) registry abstraction upgrades
  (mirror pinning, failover) validated with multi-target fixtures.
- **Dependencies:** Phase B CI/cache must exist; multi-target entries rely on
  accurate store hashes and resolver metadata.

## Phase D — Org Adoption

- **Goal:** make px deployable organization-wide with managed interpreters,
  policy hooks, audits, and legacy compatibility.
- **Acceptance criteria:** managed/pinned Python interpreters recorded in
  `px.lock`; policy/audit/SBOM/offline-prefetch hooks; optional env
  materialization (`pep582`/`venv`).
- **Exit check:** org can standardize on px with hermetic CI (managed
  interpreter + policy enforcement) while serving modern + legacy workflows.
- **Prioritized backlog:** (1) interpreter management (download, verify,
  record metadata, expose via `px env python`); (2) policy hooks, auditing,
  SBOM, offline/air-gapped preload tooling; (3) env materialization flows for
  legacy toolchains.
- **Dependencies:** needs Phase C multi-target metadata + registry hardening;
  policy/audit layers depend on deterministic artifact data.

## Phase E — GA Hardening

- **Goal:** reach GA reliability with transactional installs, recovery, lock
  v2, and public validation tooling.
- **Acceptance criteria:** transactional installs w/ rollback + cache
  integrity + recovery; Lock v2 (back-compatible) with optional signing; `px
  doctor`, migration guides, conformance/perf suites.
- **Exit check:** px survives mid-install crashes, diagnoses envs via `px
  doctor`, migrates locks safely, and passes public perf/conformance suites.
- **Prioritized backlog:** (1) transactional installer + recovery + cache
  scrubbing; (2) Lock v2 spec + signing/attestation; (3) developer tooling (`px
  doctor`, migration guides, perf/conformance automation) to meet GA bar.
- **Dependencies:** transactional install assumes mature CAS + policy hooks;
  Lock v2 must ingest old metadata; doctor/migrations rely on telemetry/error
  context from earlier phases.

## Cross-Phase Notes

- Validate continuously using the tiny sample project (`sample_px_app`) and
  future workspace fixtures to catch regressions cheaply.
- Track git checkpoints per phase milestone (see workspace-architecture agent
  output) so reviewers can audit progress easily.

## Assumptions & Open Questions

1. Runner isolation mode (`-s` vs stronger `-S`) undecided; keep bootstrap
   flexible until experiments confirm UX.
2. Formatter choice (`ruff format` vs `ruff+black`) open; Phase A interface
   should remain pluggable.
3. Lock portability (single multi-target file vs per-target splits) may evolve
   post-Phase C; parser/serializer must be extensible.
4. Single-file packaging scope TBD; avoid tight coupling until roadmap
   prioritizes it.

## Key Risks & Unknowns

- **Bootstrap surface:** CAS/runner mistakes cascade across every phase;
  invest in diagnostics + sandbox tests early.
- **Cache integrity:** unclear hashing/versioning jeopardizes transactional
  guarantees; define hash story now.
- **Cross-platform debt:** delaying Windows shims + toolchain detection will
  block Phase C/D timelines.
- **Org policy expectations:** regulatory/SBOM needs vary; gather stakeholder
  input before Phase D commitments.
