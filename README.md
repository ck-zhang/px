# px (Python eXact)

px manages Python project dependencies using **immutable environment profiles**.

Instead of treating "the environment" as a directory you activate and mutate, px builds a profile into a **global, content-addressed store** and can run directly from it. A traditional on-disk environment (venv-style) or a sandbox is optional and can be generated when needed.

## Install (from source)

```sh
cargo install --path crates/px-cli --locked
px --version
```

## Quick start

In a Python project directory:

```sh
px python install 3.12
px init
px add requests rich
px test
px run python -c "import requests, rich; print('ok')"
```

If you cloned a repo that does not use px:

```sh
px python install 3.12
px migrate --apply
px test
```

### What to commit

* `pyproject.toml` (declared intent)
* `px.lock` (resolved, exact set)

### What not to commit

* `.px/` (local state/logs). Built artifacts live under `~/.px/`.

## Common commands

* `px add <pkg>...` / `px remove <pkg>...` — update dependencies
* `px run <target> [...args]` — run commands
* `px test` — run tests
* `px sync` — ensure the local profile matches the lockfile (`--frozen` in CI)

## How px differs

### 1) Environments aren't per-project directories

px executes from a content-addressed store of built artifacts:

* no activation step
* no accidental mutation via ad-hoc installs
* identical dependency graphs can reuse the same artifacts across projects

If a directory-based layout is required (compatibility or sandboxing), px can materialize one, but it's a generated view and not the source of truth.

### 2) Native builds are pinned

When packages need compilation, px builds them inside pinned builder environments. This reduces dependence on the host machine's toolchain and helps keep CI and developer machines consistent.

### 3) Sandboxing is optional

Sandboxing is an execution mode derived from the same profile plus sandbox config:

* use it when you need container parity or system libraries
* skip it when you want the fastest local loop

## CI

```sh
px sync --frozen
px test --frozen
```

## More things you can do

### Run a pinned script from a repo snapshot (no clone, no checkout)

`px run` supports pinned GitHub and `git+...` references. This runs from a cached snapshot and does not adopt the current directory.

```sh
px run https://github.com/ck-zhang/px/blob/504a607e8d60f512f2882d9de641d56f5868a713/fixtures/run_by_reference_demo/scripts/hello.py
```

### One-file scripts with inline deps (PEP 723)

```py
# /// script
# requires-python = ">=3.11"
# dependencies = ["httpx"]
# ///
```

```sh
px run path/to/script.py
```

### Run with sandboxing (optional)

```sh
px run --sandbox python -c "import psycopg; print('ok')"
px test --sandbox
```

### Export a portable bundle

```sh
px pack app --out dist/myapp.pxapp
px run dist/myapp.pxapp
```

### Install tools separately from projects

```sh
px tool install black
px tool run black --check .
```

## Troubleshooting

* `px status` — check whether manifest / lock / profile agree
* `px explain run ...` — show what px would run (and why)
* `px why <package>` — show why a dependency is present

## Docs

* `docs/index.md`
