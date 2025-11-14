# Phase A CLI Reference

`px` is a single binary. Every command can be invoked either via grouped
subcommands (e.g., `px infra env`) or the equivalent top-level alias
(`px env`). The CLI is intentionally terse: human output is one or two lines,
while `--json` emits the envelope described below.

## Global flags

| Flag | Effect |
| --- | --- |
| `-q, --quiet` | Suppress normal stdout (errors still print to stderr). |
| `-v, --verbose` | Increase logging (stackable; `-vv` reaches trace). |
| `--trace` | Force trace logging regardless of `-v`/`-q`. |
| `--json` | Emit machine-friendly envelopes instead of plain text. |
| `--config <path>` | Optional px config file (future use). |

## UX conventions

- Every command prints a leading status line: `px <group> <command>: <summary>`.
  When additional guidance exists, it appears on a new line starting with
  `Hint:`. Human output is colorized only when stdout is a TTY and you are not
  using `-q/--json`.
- `--json` always emits a single envelope with `status`, `message`, and
  `details` keys. `status` is one of `ok`, `user-error`, or `failure`; `message`
  mirrors the human status line, and `details` is a predictable object with
  command-specific data.
- Unless explicitly noted, JSON snippets later in this document omit the
  `message` key for brevity; the real output always includes it.
- Examples:

  ```bash
  $ px cache path
  px infra cache: /home/alex/.cache/px/store

  $ px --json cache stats
  {
    "status": "ok",
    "message": "px infra cache: cache stats: 0 files, 0 bytes",
    "details": {
      "cache_path": "/home/alex/.cache/px/store",
      "cache_exists": true,
      "total_entries": 0,
      "total_size_bytes": 0
    }
  }
  ```

## Command map

- **Project:** `px project init|add|remove|install|update`
- **Install shortcut:** `px install [--frozen]` (same as `px project install`)
- **Lock:** `px lock diff|upgrade`
- **Workflow:** `px run`, `px test`
- **Quality:** `px fmt`, `px lint`, `px tidy`
- **Delivery:** `px build`, `px publish`
- **Support:** `px env {python,info,paths}`, `px cache {path,stats,prune}`
- **Workspace:** `px workspace list`, `px workspace verify`

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | User input error (bad flags, missing project files, etc.) |
| `2` | Tool failure (subprocess returned non-zero) |
| `3` | Reserved for partial success / warnings (future) |

## JSON envelope

All `--json` responses share the same shape:

```json
{
  "status": "ok",
  "command": "px infra env",
  "code": 0,
  "message": "interpreter: /usr/bin/python3\nproject root: …/px",
  "details": {
    "interpreter": "/usr/bin/python3",
    "project_root": "/home/alice/px/fixtures/sample_px_app",
    "pythonpath": "/home/alice/px/fixtures/sample_px_app",
    "env": {
      "PX_PROJECT_ROOT": "…/sample_px_app",
      "PYTHONPATH": "…/sample_px_app"
    }
  }
}
```

## Examples

All examples assume you run from `fixtures/sample_px_app` (the sample project).

### Interpreter discovery

```bash
$ cargo run -q -- env python
/usr/bin/python3
```

`px env python` honors `PX_RUNTIME_PYTHON` before falling back to
`python3`/`python` in your `$PATH`.

### Environment inspection

```bash
$ cargo run -q -- env info
interpreter: /usr/bin/python3
project root: /home/.../fixtures/sample_px_app

$ cargo run -q -- env paths
Interpreter: /usr/bin/python3
Project root: /home/.../fixtures/sample_px_app
PYTHONPATH:
  /home/.../fixtures/sample_px_app
```

Use `--json` with either mode to capture the full envelope.

### Running the sample app

```bash
$ cargo run -q -- run sample_px_app.cli -- -n PxTest
Hello, PxTest!
```

Arguments after `--` are forwarded verbatim to the Python entrypoint.

### Testing (pytest + fallback)

```bash
# tries pytest first, falls back to the builtin smoke test if missing
$ PX_TEST_FALLBACK_STD=1 cargo run -q -- test
px fallback test passed
```

If `pytest` is installed in the detected interpreter, its output is surfaced;
otherwise the builtin runner confirms `sample_px_app` still greets "Hello, World!"

### Project authoring

Project commands run from the directory that should contain `pyproject.toml`. Use
`px project …` while standing in an empty folder or an existing project.

#### Initialize a scaffold

```bash
$ mkdir -p /tmp/demo && cd /tmp/demo
$ cargo run -q -- project init --package demo_pkg
Initialized project demo_pkg
```

`--package` is required and must contain ASCII letters/numbers/underscores; `--py`
accepts a version floor (e.g., `--py 3.10` writes `>=3.10`). `--json` lists the
files created (`pyproject.toml`, package module, tests, `.gitignore`).

#### Add dependencies

```bash
$ cargo run -q -- project add requests==2.32.3
updated dependencies (added 1, updated 0)
```

Specs are dropped into `[project].dependencies` via `toml_edit`, kept sorted, and
deduplicated by package name (markers are preserved verbatim). Re-running the same
command leaves the file untouched and reports "dependencies already satisfied".

#### Remove dependencies

```bash
$ cargo run -q -- project remove requests
removed dependencies
```

Removal matches by package name regardless of pin/marker text. When nothing is
removed the command reports "no matching dependencies found". Pair with `--json`
to see which names were removed and the path to the updated `pyproject.toml`.

### Install & lockfiles

```bash
$ cargo run -q -- install
wrote /tmp/demo/px.lock

$ cargo run -q -- --json install --frozen
{
  "status": "ok",
  "message": "lockfile verified",
  "details": { "lockfile": "/tmp/demo/px.lock" }
}
```

- Phase C’s pinned-only slice accepts only exact `name==version` specs. Any
  marker, range (`>=`), or extras trigger a `user-error` explaining that pins
  are required.
- Experimental: when both `PX_RESOLVER=1` and `PX_ONLINE=1` are set, `px`
  resolves simple pure-Python ranges (for example `packaging>=24,<25`) by
  selecting the highest `py3-none-any` wheel before continuing with the
  pinned-only pipeline. The flag is opt-in; without it the legacy pin-required
  error remains.
- With that same `PX_RESOLVER=1` gate, extras and basic PEP 508 markers are
  honored. For example, `requests[socks]>=2.32 ; python_version >= "3.10"`
  resolves to the best wheel for the current interpreter and the resulting
  `px.lock` specifier retains the extras + marker text so `lock diff`/`--frozen`
  stay stable.
- `px install` queries the PyPI JSON API for each pin, picks the best wheel
  (prefer `py3-none-any`, otherwise the interpreter’s tags), downloads it to
  the px cache, and verifies the SHA256 digest before writing `px.lock`.
- When no compatible wheel exists (or `PX_FORCE_SDIST=1`), `px install`
  downloads the sdist, runs `python -m build --wheel` inside the cache, moves
  the built wheel into the deterministic cache layout, and records the
  artifact metadata in `px.lock`. The fallback requires `PX_ONLINE=1`, honors
  the same proxy/`NO_PROXY` handling as direct downloads, and is currently
  gated while we validate the rollout.
- The lock now emits v1 metadata: `[metadata].mode = "p0-pinned"` plus
  `[[dependencies]]` tables containing `name`, `specifier`, and
  `artifact.{filename,url,sha256,size,cached_path,python_tag,abi_tag,platform}`.
- `px lock upgrade` rewrites `px.lock` to schema v2, keeping the v1 dependency
  tables while adding `[[graph.nodes]]`, `[[graph.targets]]`, and
  `[[graph.artifacts]]` sections for future multi-target installs. The command
  is idempotent and never touches `pyproject.toml`.
- v1 remains the default output for `px install` until the resolver/store grow
  full graph support. Mixed repos are safe because the diff/frozen flows accept
  either version.
- `px lock diff`, `px install --frozen`, and `px tidy --frozen` normalize both
  versions before comparing, so a project can upgrade gradually without noisy
  drift reports.
- `--frozen` still surfaces drift, and it now also fails when cached wheels are
  missing or their hashes/size do not match the locked artifact data.
- Online integration tests (and the CLI examples above) expect
  `PX_ONLINE=1`; without it the network-backed tests are skipped.

### Lock diff

```bash
$ cargo run -q -- lock diff
px lock diff: clean

$ cargo run -q -- --json lock diff
{
  "status": "ok",
  "details": {
    "status": "clean",
    "added": [],
    "removed": [],
    "changed": []
  }
}
```

When `pyproject.toml` and `px.lock` diverge (new dependencies, python requirement
changes, lock schema mismatches, etc.) the command exits with `user-error`,
prints a terse summary (e.g., “`px lock diff: drift (1 added, python mismatch)`”),
and expands the JSON payload to include `added`, `removed`, `changed`,
`python_mismatch`, `version_mismatch`, and `mode_mismatch` keys so CI pipelines
can annotate builds.

### Lock upgrade

```bash
$ cargo run -q -- lock upgrade
upgraded px.lock to version 2

$ cargo run -q -- --json lock upgrade
{
  "status": "ok",
  "details": { "lockfile": "/tmp/demo/px.lock", "version": 2 }
}
```

`px lock upgrade` converts an existing v1 lock to schema v2. The command keeps
the original `[[dependencies]]` block for backward compatibility, then
materializes `[[graph.nodes]]` (name/version/marker/parents),
`[[graph.targets]]` (python/abi/platform triples), and `[[graph.artifacts]]`
entries keyed by target. It is idempotent: rerunning simply confirms the file
is already at v2. Because `px install` still writes v1 by default, you can
upgrade repositories gradually or mix versions across branches without drift
noise—diff/frozen automatically compare the normalized graph view.

Both v1 and v2 locks are normalized into the same comparable snapshot (project
metadata + sorted dependency specs + artifacts keyed by target) before diffing,
so upgrading a single member or branch will not raise false-positive drift in
mixed workspaces.

### Workspace list & verify

```bash
$ cargo run -q -- workspace list
workspace members: member-alpha, member-beta

$ cargo run -q -- --json workspace verify
{
  "status": "ok",
  "details": {
    "workspace": {
      "root": "/tmp/workspace_dual",
      "members": [
        { "name": "member-alpha", "status": "ok" },
        { "name": "member-beta", "status": "ok" }
      ]
    }
  }
}
```

- `px workspace list` inspects `[tool.px.workspace].members` from `pyproject.toml`
  and prints the normalized member names/paths; `--json` exposes
  `details.workspace.members[*].{name,path,manifest,lock_exists}` so tooling can
  diff expected vs. actual layouts.
- `px workspace verify` runs the existing lock/manifest drift detector inside
  each member. Missing manifests, missing locks, or dependency drift yield a
  `user-error` exit plus a per-member status array (`ok`, `missing-lock`,
  `drift`, etc.) so CI can point engineers at the offending package. When every
  member is clean the command exits with `status = ok`.

### Workspace install & tidy

```bash
$ cargo run -q -- workspace install
workspace install: all 2 members clean

$ cargo run -q -- --json workspace install --frozen
{
  "status": "ok",
  "details": {
    "workspace": {
      "counts": { "ok": 2, "drifted": 0, "failed": 0 },
      "members": [
        { "name": "member-alpha", "status": "verified" },
        { "name": "member-beta", "status": "verified" }
      ]
    }
  }
}

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

- `px workspace install` iterates members from `[tool.px.workspace].members`,
  running the existing per-project install logic. In offline Phase C this
  rewrites missing/out-of-date locks; `--frozen` switches to verification only
  and fails fast if any member drifts or lacks a lock.
- `px workspace tidy` is a read-only drift check that reports each member’s
  status (`tidied`, `drift`, `missing-lock`, etc.) and fails whenever a member
  needs `px install`.

### Tidy (lock drift check)

```bash
$ cargo run -q -- tidy
px quality tidy: workspace tidy

$ cargo run -q -- --json tidy
{
  "status": "ok",
  "message": "px quality tidy: workspace tidy",
  "details": { "lockfile": "/tmp/demo/px.lock" }
}
```

`px tidy` simply reports whether `px.lock` matches `pyproject`. If the manifest
changes (e.g., you edited dependencies by hand) the command exits with
`user-error` and includes a `details.drift` array so CI can print the cause.

### Cache path

```bash
$ cargo run -q -- --json cache path
{
  "status": "ok",
  "message": "px infra cache: /home/.../.cache/px/store",
  "details": {
    "path": "/home/.../.cache/px/store",
    "source": "~/.cache"
  }
}
```

Override the location by exporting `PX_CACHE_PATH=/custom/dir`; the command will
create the directory if necessary and echo the resolved absolute path.

### Cache stats & prune

```bash
$ cargo run -q -- --json cache stats
{
  "status": "ok",
  "message": "px infra cache: cache stats: 0 files, 0 bytes",
  "details": {
    "cache_path": "/tmp/px-cache",
    "cache_exists": true,
    "total_entries": 12,
    "total_size_bytes": 81920
  }
}

$ cargo run -q -- cache prune --all --dry-run
px infra cache: cache prune (dry-run): would remove 12 files (81920 bytes)

$ cargo run -q -- cache prune --all
px infra cache: cache prune: removed 12 files (81920 bytes)
```

`px cache stats` walks the resolved cache directory (honoring `PX_CACHE_PATH`) to
report file count and total bytes. `px cache prune` currently requires `--all`;
`--dry-run` previews what would be deleted, while the default mode removes every
entry under the cache root and reports how much space was reclaimed. Forgetting
`--all` surfaces a `Hint:` line reminding you to rerun with the required flag.

### Store prefetch

```bash
$ px store prefetch --dry-run
prefetch dry-run: 12 artifacts (11 hit)
```

- Reads `px.lock` and hydrates every artifact referenced by the lock into the
  px cache so CI or developer environments can run offline installs. Requires
  `PX_ONLINE=1` unless `--dry-run` is provided.
- `--workspace` walks each `[tool.px.workspace].members` entry so every member
  lock is hydrated in one command.
- Reuses the same downloader as `px install`, so proxy and `no_proxy` behavior
  stays consistent. `--dry-run` surfaces what would be fetched without touching
  the network.
- Pass `--json` to obtain per-run stats; with `--workspace` the payload adds
  per-member entries plus aggregated `workspace.totals` counts you can feed to
  dashboards.

```bash
$ px --json store prefetch --workspace
{
  "details": {
    "workspace": {
      "totals": { "requested": 4, "hit": 3, "fetched": 1 }
    }
  }
}
```
