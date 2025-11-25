# Runtimes

px manages Python interpreters explicitly so projects and tools run against known, reproducible versions.

## How px chooses a runtime

Order of precedence (deterministic):

1. `[tool.px].python` in `pyproject.toml`
2. `[project].requires-python` (PEP 621)
3. px default runtime

If no available runtime satisfies constraints, commands fail with a clear message and suggest installing one. px does not fall back to arbitrary system interpreters once a project is under px management.

## Managing runtimes

* List known runtimes: `px python list`
* Install a runtime: `px python install 3.11`
* Pin runtime for current project: `px python use 3.11` (writes `[tool.px].python`; next `px sync` rebuilds env)
* Inspect active runtimes: `px python info`

## Tips

* Tool installs can pin a runtime via `px tool install <name> --python 3.11`; the chosen version is recorded in the tool lock.
* If a runtime goes missing, px errors clearly and suggests reinstalling the required version rather than silently switching versions.
