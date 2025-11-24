# px-core architecture guardrails

This crate stays aligned with `docs/spec.md`:

- Single state machine for M/L/E lives in `px-domain`; `px-core` orchestrates transitions without redefining states (§10).
- Deterministic surfaces (runtime selection, locking, env materialization, target resolution) must not gain hidden fallbacks (§3.4).
- Reader commands (`run`, `test`, `fmt`, `status`, `why`) must never mutate manifest or lock; only `run`/`test` may repair envs in dev (§10.5).
- Non-TTY/`--json` output remains stable and spinner-free (§8.4).

## Package boundaries (allowed deps)
- `config`: config parsing, defaults, env snapshotting. Depends on `px-domain` only.
- `python`: interpreter discovery, marker env detection, Python process helpers. Depends on `config`.
- `store`: cache layout, hashing, wheel/sdist extraction. Depends on `config`; **no** `runtime` back-edge.
- `distribution`: build/publish orchestration, artifact formatting/validation. Depends on `store`, `python`, `config`.
- `project`: project lifecycle flows (`init`/`add`/`remove`/`sync`/`update`/`status`/`why`). Depends on `runtime` plus lower packages.
- `migration`: onboarding/migrate flows. Depends on `runtime` plus lower packages.
- `tools`: px-managed tool install/run/list/remove/upgrade flows. Depends on `runtime` plus lower packages.
- `runtime`: run/test orchestration only (planning/executors/process glue). Depends on `config`, `python`, `store`, `distribution`.
- `tooling`: shared CLI messages, diagnostics, progress/log plumbing; may depend on lower packages but owns no business logic.
- `lib` facade: re-exports the public API used by `px-cli`; everything else stays `pub(crate)`.

Future boundary checks should keep imports within these rules and avoid creating new top-level crates.

Boundary tests currently enforce:
- `store` stays out of `runtime` and `distribution`.
- `distribution` stays out of `runtime`.
- `python` stays out of `store`, `distribution`, and `runtime`.
- `tooling` stays out of `runtime`.
