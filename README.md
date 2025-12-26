# px - Python eXact

px manages Python project dependencies using **immutable environment profiles**.

Instead of treating "the environment" as a directory you activate and mutate, px builds a profile into a **global, content-addressed store** and can run directly from it. A venv-style on-disk env or a sandbox is optional and generated on demand.

## Install from source

```sh
cargo install --path crates/px-cli --locked
px --version
```

## Try it without cloning

If you don't have a px-managed Python runtime yet, px will offer to install one on first use. This demo script requires Python >=3.12.

Run a script straight from this repo's `HEAD`. Default is commit-pinned; `--allow-floating` allows `HEAD` and branches:

```sh
px run --allow-floating https://github.com/ck-zhang/px/blob/HEAD/fixtures/run_by_reference_demo/scripts/whereami.py
```

Run the same script in a Linux sandbox. Requires podman or docker; first run may take longer:

```sh
px run --allow-floating --sandbox https://github.com/ck-zhang/px/blob/HEAD/fixtures/run_by_reference_demo/scripts/whereami.py
```

## Quick start

In a Python project directory:

```sh
px init
px add requests rich
mkdir -p tests
cat > tests/test_smoke.py <<'PY'
def test_smoke():
    import requests, rich
    assert True
PY
px test
px run python -c "import requests, rich; print('ok')"
```

If you cloned a repo that does not use px:

```sh
px migrate --apply
px test
```

### What to commit

* `pyproject.toml` - declared intent
* `px.lock` - resolved, exact set

### What not to commit

* `.px/` - local state and logs. Built artifacts live under `~/.px/`.

## Common commands

* `px add <pkg>...` / `px remove <pkg>...` - update dependencies
* `px run <target> [...args]` - run commands
* `px test` - run tests
* `px sync` - ensure the local profile matches the lockfile; use `--frozen` in CI

## How px differs

### 1) Environments aren't per-project directories

px executes from a content-addressed store of built artifacts:

* no activation step
* no accidental mutation via ad-hoc installs
* identical dependency graphs can reuse the same artifacts across projects

If you need a directory-based layout for compatibility or sandboxing, px can materialize one. It's a generated view and not the source of truth.

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

### One-file scripts with inline deps, PEP 723

```py
# /// script
# requires-python = ">=3.11"
# dependencies = ["httpx"]
# ///
```

```sh
px run path/to/script.py
```

### Run with sandboxing

```sh
px run --sandbox python -c "print('ok')"
px test --sandbox
```

### Export a portable bundle

```sh
px pack app --out dist/myapp.pxapp
px run dist/myapp.pxapp
```

Tip: for library-only projects, set an entrypoint with `px pack app --entrypoint "python -m your_module"`.

### Install tools separately from projects

```sh
px tool install black
px tool run black --check .
```

## Troubleshooting

* `px status` - check whether manifest / lock / profile agree
* `px explain run ...` - show what px would run and why
* `px why <package>` - show why a dependency is present

## Docs

* `docs/index.md`
