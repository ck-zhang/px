# px Modularization Plan

This plan shapes the workspace into three crates while making `px-core` modular on the inside. It must uphold the global design in `docs/spec.md`, especially the single project state machine (§10), deterministic surfaces (§3.4), and the strict split between readers and writers for commands (§10.5/§10.8).

## Scope and constraints
- Keep exactly three crates: `px-domain`, `px-core`, `px-cli`.
- Avoid new top-level crates; prefer intra-crate packages with tight `pub(crate)` boundaries.
- Preserve UX and contracts defined in `docs/spec.md` (error shapes, JSON/non-TTY rules, runtime selection, command invariants).

## Spec anchors to protect
- **State machine ownership**: M/L/E identities, `manifest_clean` and `env_clean`, and allowed transitions live in `px-domain` (pure types and algorithms). `px-core` orchestrates transitions but must not reinvent state classification (§10.1–§10.8).
- **Deterministic surfaces**: Runtime selection, locking, env materialization, target resolution, and error/JSON output must remain deterministic (§3.4, §8.4). Module seams must not hide implicit fallback logic.
- **Command roles**: Mutable commands (`init`, `add`, `remove`, `sync`, `update`, `migrate --apply`) own M/L/E writes; reader commands (`run`, `test`, `fmt`, `status`, `why`) never mutate manifests or locks (only optional env repair for `run`/`test` in dev) (§4, §10.5).
- **Non-goals**: No new user-facing concepts beyond spec (§9); modularization must not introduce “workspace”/“cache” commands or plugin hooks.

## Architecture target
### Crates
- **px-domain**: Pure data and algorithms: state machine types, manifest/lock/env identity, deterministic helpers (runtime precedence, target resolution), error codes. No IO or process spawning.
- **px-core**: Internal packages with narrow `pub(crate)` APIs; re-export a small surface from `lib.rs` for `px-cli`.
- **px-cli**: Argument parsing, user IO, and presentation only; depends solely on the `px-core` facade.

### px-core packages and allowed deps
- **config**: Config parsing, defaults, env snapshotting. Depends on `px-domain` only.
- **python**: Interpreter discovery, marker env detection, process helpers. Depends on `config`.
- **store**: Cache layout, hashing, wheel/sdist extraction. Depends on `config`; no dependency on `runtime`.
- **distribution**: Build/publish orchestration, artifact formatting/validation. Depends on `store`, `python`, `config`.
- **runtime**: `run`/`test` planning and process orchestration. Depends on `config`, `python`, `store`, `distribution`. No back-edge into `tooling`.
- **tooling**: Shared CLI-facing messages, diagnostics, progress/logging plumbing; no business logic. Can depend on any lower package but exposes no side effects on its own.
- **lib facade**: Re-exports only the public API (`px-cli` needs). Everything else `pub(crate)`.

Dependency rules are enforced by convention first, then by boundary tests (see Phase 2).

## Current hot spots to split
- `crates/px-core/src/lib.rs` (~2.3k LOC): mixes routing, constants, and tests; should shrink to a facade plus thin module wiring.
- `distribution/build.rs` (~700 LOC) and `distribution/publish.rs` (~600): blend planning, IO, and helpers; need separation of plan vs execution vs formatting.
- `run.rs` and `fmt_runner.rs` (~600 each): combine planning, process wiring, and user messaging; user-facing strings should move to `tooling`.
- `store/mod.rs` (~380 LOC) plus wheel/sdist/cache code in one layer: cache layout vs extraction vs metadata should be distinct modules.

## Phased execution
### Phase 0 — guardrails
- Keep clippy/tests green; add doc-comments to public items that remain part of the `px-core` facade.
- Document allowed intra-`px-core` dependencies (matrix above) next to the code.
- Add quick tests for spec-critical invariants imported from `px-domain` (state classification, deterministic runtime selection inputs).

### Phase 1 — untangle large files (no crate moves)
- Move tests out of `lib.rs` into module-level `tests` or `tests/` integration.
- Split `distribution/build.rs` into `plan.rs` (target selection per §4.4), `build.rs` (execution), `artifacts.rs` (formatting/hashing per §2.3/§8.4).
- Split `distribution/publish.rs` into `plan.rs` (artifact selection/rules) and `publish.rs` (IO/upload).
- Split `run.rs` and `fmt_runner.rs` into planner vs executor modules; lift user-visible strings/log shapes into `tooling` to keep determinism (§8.4) centralized.
- In `store`, separate cache layout/indexing from wheel/sdist extraction and metadata; keep `mod.rs` as a thin facade.

### Phase 2 — enforce boundaries inside `px-core`
- Create subdirectories (`core/config`, `core/python`, `core/store`, `core/distribution`, `core/runtime`, `core/tooling`) and move modules accordingly.
- Mark cross-area APIs `pub(crate)`; expose only a minimal re-export set from `lib.rs` that matches the CLI needs.
- Add boundary tests or a compile-time deny-list (e.g., `cfg(test)` module that asserts disallowed imports) to prevent cycles that violate the dependency rules.

### Phase 3 — API polishing
- Trim `lib.rs` to a facade (<200 LOC) plus re-exports and constants.
- Add boundary-focused integration tests that exercise only the public `px-core` API used by `px-cli` (matching command contracts in §10.5/§10.8).
- Prune dead helpers; re-run clippy with `-D warnings`.

## Practices to keep architecture clean
- Each package owns its errors; convert at handoff points rather than sharing a mega error type (aligns with “What/Why/Fix” in §8.1).
- Keep serialization in the right layer: domain data in `px-domain`; config persistence in `config`.
- Process/IO helpers live behind traits so tests can mock without touching filesystem/network.
- Periodically run module-graph checks (`cargo modules`/`cargo deps`) to flag new cross-area imports.

## Success criteria
- Only three crates remain; `px-cli` depends solely on the `px-core` facade.
- `px-core/src/lib.rs` is a thin facade; largest source file <400 LOC and most modules <200 LOC.
- Rebuilding packaging changes does not force recompiling runtime/config-heavy modules.
- Observable behavior continues to satisfy `docs/spec.md` (state machine invariants, deterministic surfaces, non-TTY/JSON rules).
