# Status Report

## Summary

- Phase A CLI flows (project init/add/remove, env/run/test, cache) are wired
  through `crates/px-cli/src/main.rs` and handled in
  `crates/px-core/src/lib.rs`; integration coverage lives under
  `crates/px-cli/tests/*.rs` and the `fixtures/sample_px_app` project.
- Phase B foundations (lock diff, cache path/stats/prune) plus workspace-level
  install/tidy/list/verify are online; the pinned-only install path now hits
  PyPI, but a full resolver/store, editable installs, and Windows parity from
  `docs/requirements.md` remain open.
- Resolver extras + markers slice: when `PX_RESOLVER=1` is set we parse
  `requests[socks]`-style requirements with basic PEP 508 markers, evaluate
  them against the detected interpreter, and record normalized specifiers in
  both lock v1 and v2 so diff/--frozen remain deterministic.
- Human output now uses consistent `px <group> <command>:` prefixes, hint
  blocks, and optional color-on-TTY; JSON envelopes are standardized to
  `{status,message,details}` for every command.
- Phase C adds `[tool.px.workspace]` parsing, workspace fixtures, and
  orchestration commands, yet multi-target locks, per-target caches, and
  registry abstractions are still TODO.
- `px lock upgrade` now rewrites v1 locks to schema v2 (graph nodes/targets/
  artifacts) while keeping the legacy dependency tables; diff/frozen accept
  either version so repos can migrate gradually.
- `px store prefetch` now supports `--workspace`, hydrating every member lock
  in one run; `crates/px-cli/tests/prefetch.rs` and
  `tests/prefetch_workspace.rs` cover single-project and workspace cache
  priming end-to-end.
- Overall: CLI UX + pinned lock workflows are validated; completing the
  roadmap requires building the resolver/store (beyond exact pins), cache
  backends, and multi-target metadata before org adoption (Phases D/E).

## Phase A/B/C – Done vs TODO

- **Phase A (docs/requirements.md:9-24)**
  - [ ] Resolver + PyPI client + wheel install (blocked on px-store/
        px-resolver implementation).
  - [x] CLI scaffolding for `px init/add/remove/install/run/test/fmt/lint/
        build/publish/cache/env/tidy` (`crates/px-cli/src/main.rs`).
  - [x] Deterministic lock v1 output + `--frozen` enforcement (pinned-only
        installs, cached-wheel verification) with coverage in
        `fixtures/sample_px_app` + `crates/px-cli/tests/lock.rs`.
  - [ ] CAS store + `.px/site` bootstrap semantics (doc-only in
        `docs/architecture.md`, not yet implemented).
- **Phase B (docs/requirements.md:27-44)**
  - [x] Lock diff command + JSON envelope (`px lock diff`).
  - [x] Cache path/stats/prune + overrides (env + tests under
        `crates/px-cli/tests/cache.rs`).
  - [ ] CI-friendly cache priming/offline install across platforms (Linux-only
        prototype; no Windows shims yet).
  - [ ] VCS/path deps + IDE interpreter shims (not implemented).
- **Phase C (docs/requirements.md:47-68)**
  - [x] `[tool.px.workspace]` parsing + workspace commands (list/verify/install/
        tidy) with fixtures `fixtures/workspace_dual` and tests
        `crates/px-cli/tests/workspace.rs`.
  - [ ] Multi-target lock entries + per-target wheel caches (lock is single-
        target, see `crates/px-core/src/lib.rs`).
  - [ ] Registry/mirror abstraction + toolchain detection (future work).

## Tests & Fixtures Coverage

- `fixtures/sample_px_app`: exercised by `crates/px-cli/tests/workflows.rs`,
  `tests/lock.rs`, and `tests/project.rs` (init/add/remove/install/run/test).
- `fixtures/workspace_dual`: used by `crates/px-cli/tests/workspace.rs` for
  list/verify/install/tidy flows and by `tests/prefetch_workspace.rs` for
  workspace cache hydration + dry-run JSON summaries.
- Cache/env integration: `crates/px-cli/tests/cache.rs` and the env/path tests
  in `tests/workflows.rs` validate overrides, JSON output, and interpreter
  detection.
- Remaining gaps: no integration tests for resolver/store, Windows shims, or
  multi-target lock scenarios.

## Risks / Unknowns

- Resolver/store still incomplete: the current pinned-only path handles
  `name==version` + wheel downloads, but editable/VCS deps and multi-target
  resolution await real `px-resolver`/`px-store` plumbing.
- Cache/CAS integrity story undefined; lack of hashing means transactional
  guarantees (Phase E) cannot be demonstrated.
- Windows and multi-platform coverage absent; Phase B/C acceptance criteria
  call for parity and toolchain detection.
- Org/policy requirements (Phases D/E) hinge on metadata/signing work that has
  not started; scope creep risks schedule slip.

## Next Steps

- **Lock v2 migration**
  - Land the `px lock upgrade` workflow + docs (done) and encourage teams to
    adopt it before multi-target resolution ships.
  - Track mixed v1/v2 usage via CI `--json` envelopes and ensure diff/frozen
    continue to normalize both formats.
- **Phase A – Hardening**
  - Expand integration coverage for core CLI flows (S, dep: existing harness;
    quick win: reuse `fixtures/sample_px_app`).
  - Guard config parsing against missing env values (S, dep: config plumbing;
    quick win: default fallbacks).
  - Harden logging/error surfaces so failures emit telemetry-friendly data (M,
    dep: logging crate update).
  - Audit CI for flakiness and add retries (M, dep: workflow updates; quick win:
    rerun-only target).
- **Phase B – Polish**
  - Refine CLI help/examples for new flags (S, dep: `docs/spec.md`).
  - Improve `px apply` (future) output for clearer progress cues (M, dep:
    formatter refactor).
  - Align README/quickstart with actual behavior (S, quick win: sync
    `docs/quickstart.md`).
  - Add smoke test ensuring `cargo fmt`/`cargo clippy` run in CI (M, dep:
    toolchain pinning).
- **Phase C – Extensions**
  - Add `px workspace` summary/report subcommand (M, dep: workspace metadata).
  - Define plugin hook for third-party linters (L, dep: command dispatch).
  - Prototype diagnostics dashboard backed by telemetry (L, dep: telemetry svc).
- **Phase D/E – Future**
  - Document multi-repo orchestration roadmap (S, quick win: extend
    `docs/architecture.md`).
  - Prototype GH Actions helper for `px sync`/workspace verification (M, dep:
    CLI extension API).
  - Evaluate web UI for telemetry dashboard (L, dep: Phase C dashboard POC).
