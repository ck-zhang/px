# PX Readiness

## Supported Today

- **Onboarding + auto-pin:** `px migrate --write` backfills `pyproject.toml`,
  snapshots `.px/onboard-backups/`, and auto-pins loose specs in the base
  dependencies plus the `px-dev` optional group when markers apply.
- **Lock + install:** `px install` regenerates `px.lock`, refreshes `.px/site`,
  and surfaces drift via `px install --frozen` and `px tidy`. `px lock diff`
  reports schema / dependency mismatches, while `px lock upgrade` emits the v2
  graph when it exists.
- **Runner UX:** `px run <module|script>` infers defaults from
  `[project.scripts]` or `<package>.cli`, falling back to python/path
  passthrough. `px test` shells out to `pytest` with a builtin smoke fallback.
- **Workspace/install hygiene:** `px workspace [list|verify|install|tidy]`
  iterates members declared under `[tool.px.workspace]` and reuses the same
  lock/tidy/install plumbing per project.
- **Cache + store:** `px cache path/stats/prune --all` and `px store prefetch`
  manage the shared wheel cache and lock-driven prefetching for individual
  projects or the entire workspace.
- **Build artifacts:** `px output build` produces sdists/wheels directly from
  `pyproject`. `px output publish --dry-run` enumerates artifacts + env
  requirements (but does not push anything yet).

## Current Gaps

- Autofix only touches `[project.dependencies]` and the `px-dev` optional group;
  all other extras must be curated manually.
- Resolver now runs by default (disable with `PX_RESOLVER=0`), so pins with
  extras/markers succeed end-to-end, but URL requirements are still rejected.
- Publish is a stub: after verifying `PX_ONLINE` + token env, the handler only
  prints success.
- Workspace onboarding lacks `px workspace migrate`, so every member must run
  migrate/install by hand.
- Tests and some CLI suites hit live PyPI; without `PX_ONLINE=1` they fail.

## Python Package Manager Checklist

| Capability | Status | Notes |
| --- | --- | --- |
| pyproject onboarding & autopin | [x] | `px migrate --write` + backup manager handle single projects with prod/dev scopes |
| requirements parser (markers/comments) | [x] | `requirements*.txt` parsing strips comments and respects markers during autopin |
| dependency resolver for ranges | [~] | Resolver runs by default (set `PX_RESOLVER=0` to disable) and auto-pins bare names/ranges/extras/markers; URLs still unsupported |
| pinned installs + lockfile authoring | [x] | `px install` enforces `name==version`, renders `px.lock`, refreshes `.px/site` |
| installer extras / URL support | [~] | Extras/markers now flow through pinned installs; URL deps remain rejected |
| marker awareness in installs | [x] | Resolver + install honor markers when pinning/spec normalization |
| lock verification / drift detection | [x] | `px install --frozen`, `px tidy`, `px lock diff` reuse shared drift analyzers |
| wheel cache + store | [x] | `px-store` downloads/caches wheels, supports prefetch via lock metadata |
| workspace support | [~] | Install/tidy/verify implemented; migrate/update automation missing |
| script/test runner | [x] | `px run`/`px test` wrap python/pytest with passthrough fallbacks |
| build artifacts (sdist/wheel) | [x] | Minimal sdists/wheels emitted via `px output build` |
| publish pipeline | [ ] | No upload; dry-run only prints metadata |
| cache pruning | [x] | `px cache prune --all` removes cached wheels with dry-run support |
| offline/install replay | [~] | Works when lock artifacts exist locally; resolver/install still fetch wheels |
| extras/groups management | [ ] | Only `px-dev` optional group is automated |
| URL/VCS dependencies | [ ] | Explicitly rejected in resolver + install |

Legend: `[x]` implemented, `[~]` partial, `[ ]` missing.

## 1–2 Minute Demo Script

```bash
# Sample project (fixtures/sample_px_app)
cd fixtures/sample_px_app
../../target/debug/px migrate --write --allow-dirty
../../target/debug/px install
../../target/debug/px run sample_px_app.cli -- -n Demo
../../target/debug/px test

# Real repo smoke (/home/toxictoast/test/black)
cd /home/toxictoast/test/black
RUST_BACKTRACE=1 px migrate --write --allow-dirty
RUST_BACKTRACE=1 px --json migrate --write --allow-dirty > migrate.json
cat migrate.json
```

## Next Up

- Finish URL/VCS handling in resolver/install so direct references work.
- Implement `px workspace migrate` (and friends) to batch onboarding.
- Replace the publish stub with a real upload path (twine API or warehouse API).
