# Quickstart (Phase A sample)

The repo ships with `fixtures/sample_px_app`, a tiny greeting package used to
exercise the Phase A CLI. The steps below assume you have `cargo` and Python
3.12+ available on your PATH.

```bash
git clone https://example.com/px.git
cd px
cd fixtures/sample_px_app
```

All commands below are run from this sample project directory.

> **Note:** JSON fragments in this guide show the `status`/`details` payloads.
> The live CLI also includes the full `message` field described in the CLI
> reference.

## 1. Discover the interpreter

```bash
$ cargo run -q -- env python
px infra env: interpreter /usr/bin/python3
```

If you prefer a different interpreter, point `PX_RUNTIME_PYTHON` at it:
`PX_RUNTIME_PYTHON=$HOME/.pyenv/versions/3.12.3/bin/python cargo run …`

## 2. Inspect env metadata

```bash
$ cargo run -q -- env info
px infra env: interpreter: /usr/bin/python3
project root: /home/.../fixtures/sample_px_app

$ cargo run -q -- env paths
px infra env: interpreter /usr/bin/python3
project root: /home/.../fixtures/sample_px_app
PYTHONPATH:
  /home/.../fixtures/sample_px_app
```

Use `--json` to capture the same data programmatically.

## 3. Run the package

`px run` infers the lone `[project.scripts]` entry (`sample_px_app.cli`), so you
can omit `ENTRY` and still forward args after `--`:

```bash
$ cargo run -q -- run -- -n PxTest
Hello, PxTest!

$ cargo run -q -- --json run -- -n PxTest
{
  "status": "ok",
  "message": "px workflow run: Hello, PxTest!",
  "details": {
    "mode": "module",
    "entry": "sample_px_app.cli",
    "defaulted": true,
    "args": ["-n", "PxTest"]
  }
}
```

Need to run a raw executable instead? Put it before the separator: `cargo run -q
-- run python -- -m sample_px_app.cli -n PxAlt`.

## 4. Run the tests (pytest or fallback)

```bash
$ PX_TEST_FALLBACK_STD=1 cargo run -q -- test
px fallback test passed
```

If `pytest` is installed in the detected interpreter it will run the sample
tests instead; setting `PX_TEST_FALLBACK_STD=1` forces the builtin smoke test
used in our CI examples.

## 5. Inspect the cache location

```bash
$ cargo run -q -- --json cache path
{
  "status": "ok",
  "message": "px infra cache: /home/.../.cache/px/store",
  "details": { "path": "/home/.../.cache/px/store", "source": "~/.cache" }
}
```

Override the cache root with `PX_CACHE_PATH=/tmp/px-store`; the directory will
be created on demand.

## 6. Bootstrap a new project

From anywhere outside the repo (e.g., `/tmp`), create a scratch directory and
run the project commands from there:

```bash
$ mkdir -p /tmp/px-demo && cd /tmp/px-demo
$ cargo run -q -- project init
px project init: initialized project px_demo
Hint: Pass --package <name> to override the inferred module name.

$ cargo run -q -- project add requests==2.32.3
px project add: updated dependencies (added 1, updated 0)

$ cargo run -q -- project remove requests
px project remove: removed dependencies

$ cargo run -q -- run
Hello, World!
```

- `--package` is optional. By default px sanitizes the directory name (e.g.,
  `/tmp/px-demo` → `px_demo`).
- `--py 3.11` (or similar) overrides the default `>=3.12` requirement.
- Dependency specs are inserted into `[project].dependencies`, sorted, and kept
  unique by name. Removing a name ignores version pins/markers.

Use `--json` with these commands to capture the scaffold file list or which
dependencies changed. When testing repeatedly, delete the temp directory or
re-run `px project init` in a fresh folder to avoid the "pyproject.toml already
exists" guardrail.

## 7. Manage the lockfile

From any project directory (fixture or bootstrap):

```bash
$ cargo run -q -- install
px project install: wrote /tmp/px-demo/px.lock

$ cargo run -q -- --json install --frozen
{
  "status": "ok",
  "message": "px project install: lockfile verified",
  "details": { "lockfile": "/tmp/px-demo/px.lock" }
}

$ cargo run -q -- --json tidy
{
  "status": "ok",
  "message": "px quality tidy: px.lock matches pyproject",
  "details": { "lockfile": "/tmp/px-demo/px.lock", "status": "clean" }
}
```

- `px sync` now resolves bare names, ranges, extras, and markers
  automatically (set `PX_RESOLVER=0` to return to the legacy
  `name==version` enforcement) and then writes the pinned specifiers back to
  `pyproject.toml`.
- Extras and basic PEP 508 markers are normalized during pinning so the
  resulting `px.lock` carries the same metadata that `px lock diff`/`px sync
  --frozen` compare against.
- The command queries the PyPI JSON API for every pin, selects a compatible
  wheel (preferring `py3-none-any`), downloads it into the px cache, verifies
  the SHA256 digest, and records the artifact metadata inside `px.lock`.
- When PyPI lacks a compatible wheel (or `PX_FORCE_SDIST=1`), `px sync`
  fetches the sdist, builds a wheel via `python -m build --wheel` inside the
  cache, stores the result in the deterministic cache layout, and records the
  artifact in `px.lock`. The fallback needs `PX_ONLINE=1`, reuses the same
  proxy/`NO_PROXY` handling as normal downloads, and stays gated while it is
  hardened.
- `px.lock` v1 includes `[[dependencies]]` tables with `name`, `specifier`, and
  `artifact.{filename,url,sha256,size,cached_path,python_tag,abi_tag,platform}`
  plus `[metadata].mode = "p0-pinned"`.
- `--frozen`/`px tidy` still refuse to rewrite the lock, and now they also fail
  when cached wheels are missing or checksums don’t match the lock.
- Set `PX_ONLINE=1` when running these examples or the online integration
  tests; without it `cargo test` skips the network-backed cases.
- `--frozen` and `px tidy` both fail (exit code 1) when the lock is missing or
  out of sync, and their JSON envelopes include `details.drift` so CI can show
  the reason. Fix drift by re-running `px sync`.

## 8. Inspect diffs and cache usage

```bash
$ cargo run -q -- lock diff
px lock diff: clean

$ cargo run -q -- --json cache stats
{
  "status": "ok",
  "message": "px infra cache: cache stats: 0 files, 0 bytes",
  "details": { "total_entries": 0, "total_size_bytes": 0 }
}

$ cargo run -q -- cache prune --all --dry-run
cache prune (dry-run): would remove 0 files (0 bytes)

$ cargo run -q -- lock upgrade
upgraded px.lock to version 2
```

- Use `px lock diff --json` whenever CI needs to confirm that `pyproject` and
  `px.lock` still agree. Drift yields `user-error` with structured details.
- `px cache stats` reports how many files live under the resolved cache root
  (honoring `PX_CACHE_PATH`).
- `px cache prune` currently wipes the entire cache; pass `--all --dry-run` to
  see what would be deleted, then rerun without `--dry-run` once you’re ready to
  reclaim the space.
- `px store prefetch` hydrates the cache from `px.lock`; add `--workspace` to
  warm every member in a single run. Keep `PX_ONLINE=1` unless `--dry-run`.
- `px store prefetch --workspace --json` emits per-member stats and aggregated
  `details.workspace.totals` so CI dashboards can confirm caches are hydrated.
- `px lock upgrade` rewrites the lock to schema v2 (graph nodes/targets/
  artifacts) while leaving the v1 dependency block in place for tooling that
  still expects it. Rerunning is safe and only updates timestamps.
- `px sync` keeps writing v1 by default, so it is fine to mix lock versions
  across branches. `px lock diff`, `px sync --frozen`, and `px tidy` accept
  both formats and compare the normalized dependency graph.

## 9. Migrate an existing project

`px migrate` lets you trial px on an existing repo without touching files. It
reads `pyproject.toml`, `requirements.txt`, and `requirements-dev.txt`, then
prints the detected dependencies so you can decide whether to commit.

Flags: `--dry-run` (default preview), `--write` (apply + backup), `--yes`,
`--no-input`, `--source <path>`, `--dev-source <path>`, `--allow-dirty`,
`--lock-only`, `--no-autopin`, and `--json` for envelopes.

Status lines and the single hint use a muted palette when stdout is a TTY.
Pass `--no-color` (or set `NO_COLOR=1`) to disable styling; JSON output is
never colorized.

```bash
$ cargo run -q -- migrate --source requirements.txt
px migrate: plan ready (prod: 2, dev: 0, sources: 1, project: requirements)
Hint: Preview confirmed; rerun with --write to apply

Package  Source            Requested        Scope
-------  ----------------  ---------------  -----
rich     requirements.txt  rich==13.7.1     prod
uvloop  requirements.txt  uvloop==0.20.0  prod
```

```json
{
  "status": "ok",
  "command": "px migrate",
  "details": {
    "project_type": "requirements",
    "packages": [
      { "name": "rich", "requested": "rich==13.7.1", "scope": "prod" }
    ],
    "write_requested": false,
    "dry_run": true
  }
}
```

Add `--write` once you are satisfied with the preview. px refuses to run when
`git status` is dirty (pass `--allow-dirty` to override), backs up touched files
under `.px/onboard-backups/<timestamp>/`, merges requirements into
`pyproject.toml` unless `--lock-only`, and writes `px.lock` via the normal
install flow.

```bash
$ cargo run -q -- migrate --write --source requirements.txt
px migrate: plan applied (prod: 2, dev: 0)
Hint: Backups stored under .px/onboard-backups/20251114T120001
```

```json
{
  "status": "ok",
  "details": {
    "actions": {
      "pyproject_updated": true,
      "lock_written": true,
      "backups": [
        ".px/onboard-backups/20251114T120001/pyproject.toml"
      ]
    },
    "write_requested": true,
    "dry_run": false
  }
}
```

`px migrate --write` also auto-pins any loose requirements (`>=`, `<`, etc.).
Pinned entries show up in `details.autopinned`, and the hint callout includes a
summary so you know what changed. Pass `--no-autopin` if you prefer to edit the
specs manually before re-running.

Marker-aware filtering keeps this safe: only specs whose PEP 508 markers match
the active interpreter are pinned. Marker-mismatched entries (say,
`tomli>=1.1.0; python_version<'3.11'` when running on Python 3.13) stay in the
file as-is and are not reported as missing pins.

```json
{
  "status": "ok",
  "details": {
    "autopinned": [
      {
        "name": "packaging",
        "from": "packaging>=23.0",
        "to": "packaging==23.2",
        "scope": "prod"
      }
    ]
  }
}
```

## 10. Build & publish

`px build` creates tarballs and wheels under `build/`. Use `PX_SKIP_TESTS=1`
to skip project tests before packaging. Dry runs show what would be written;
a real run reports the first artifact’s size and truncated sha256.

```bash
$ PX_SKIP_TESTS=1 px build
px build: wrote 2 artifacts (755 B, sha256=bc77…)
```

`px publish --dry-run` lists the artifacts and registry target. Real uploads
require `PX_ONLINE=1` and credentials (default token env `PX_PUBLISH_TOKEN`).
Missing gates return actionable `user-error`s.

```bash
$ px publish --dry-run
px publish: dry-run to pypi (2 artifacts)
```

## 11. Migration checklist

1. Start from a clean `px.lock` (`cargo run -q -- lock diff`).
2. Upgrade the schema (`cargo run -q -- lock upgrade`).
3. Re-verify artifacts (`cargo run -q -- install --frozen`).
4. Update CI/docs references if they mention the lock version.

## 12. Explore workspaces

The repo ships with `fixtures/workspace_dual`, a workspace that references two
members (`member_alpha`, `member_beta`). Copy it to a scratch directory and run
the workspace commands from the root:

```bash
$ cp -R fixtures/workspace_dual /tmp/workspace_dual
$ cd /tmp/workspace_dual
$ cargo run -q -- workspace list
workspace members: member-alpha, member-beta

$ cargo run -q -- --json workspace verify
{ "status": "ok", ... }

# break a member by removing its lock
$ rm member_beta/px.lock
$ cargo run -q -- workspace verify
px workspace verify: drift in member-beta (px.lock missing)
Hint: run `px workspace sync` or `px sync` inside drifted members

# repair the drifted member
$ cargo run -q -- workspace sync
workspace sync: all 2 members clean

$ cargo run -q -- --json workspace sync --frozen
{ "status": "ok", "details": { "workspace": { "counts": { "ok": 2 }, ... } } }

$ cargo run -q -- --json workspace tidy
{
  "status": "ok",
  "details": {
    "workspace": {
      "members": [ { "name": "member-alpha", "status": "tidied" }, ... ]
    }
  }
}
```

- `px workspace list --json` enumerates `[tool.px.workspace].members` so you can
  script against the `details.workspace.members` array.
- `px workspace verify` fails when any member is missing its manifest/lock (or
  the lock is stale). Re-running `px sync` inside the affected member(s)
  restores a clean state.
- `px workspace sync` rewrites missing/out-of-date locks for every member.
  Pass `--frozen` to verify the entire workspace without touching the files.
- `px workspace tidy` is a read-only drift check that fails when any member is
  missing its lock or has drift; run it after bulk installs to confirm everything
  is still clean.

## Troubleshooting

- **Missing interpreter:** ensure Python 3.12+ is installed, or point
  `PX_RUNTIME_PYTHON` at an existing binary.
- **Custom caches:** set `PX_CACHE_PATH` if you want the CAS store under a
  different directory (helpful in CI).
- **Running outside the sample project:** Phase A expects commands to be run
  from a project root that contains `pyproject.toml`. Use `cd fixtures/sample_px_app`
  before invoking `px`.
