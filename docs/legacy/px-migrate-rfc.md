# RFC: `px migrate`

## Summary

Introduce a first-class `px migrate` command that turns existing Python
projects into px-managed projects with minimal ceremony. The command detects
the current project shape (`pyproject.toml`, `requirements*.txt`, or bare
directories), imports dependencies, pins them, writes `px.lock`, and optionally
scaffolds `pyproject.toml`. It supports interactive confirmations and
non-interactive CI-friendly flags, dry-run previews, backups, and structured
`--json` output.

## Current Entry-Point Friction

- `px project init` only scaffolds greenfield projects; existing repos must
  manually edit `pyproject.toml` before px can help (docs/cli.md:143-178).
- `px sync` historically rejected ranges/extras unless `PX_RESOLVER=1` was
  set; the resolver now runs by default (set `PX_RESOLVER=0` to opt out), so
  migrate should lean on that behavior to keep users from hand-pinning
  (docs/cli.md:194-206).
- The quickstart states px expects to run from a directory that already owns a
  `pyproject.toml`, offering no path for requirements-only repos
  (docs/quickstart.md:257-265).
- Docs lack guidance for importing `requirements.txt` / `requirements-dev.txt`
  or for backing up existing manifests before px touches them. Users face a
  hurdle before they can evaluate the tool.

## Goals

1. Convert `requirements*.txt` or loosely pinned `pyproject.toml` inputs into a
   pinned `px.lock` in one command.
2. Offer a “buttery smooth” UX: clear previews, safe defaults, minimal prompts.
3. Support both interactive shells (prompts) and automated environments
   (non-interactive flags, deterministic output, `--json`).
4. Never clobber user data without consent: backups, git-dirty guards, dry-run.

## Non-Goals

- Replacing the existing resolver; `px migrate` will call the same resolution
  pipeline used by `px sync` (which can still be disabled via
  `PX_RESOLVER=0`).
- Full Poetry/uv lock import (future work once the base command ships).

## UX Patterns to Borrow

Research across Poetry, pip-tools, and uv suggests the following patterns:

- Detect and explain what will change before writing (Poetry’s explicit add).
- Resolve to deterministic pins and show a delta table like pip-tools.
- Provide `--dry-run` that performs the full resolution yet skips writes.
- Validate hashes / consistency up front and warn before writing locks.
- Offer ergonomic sub-flags (`--lock-only`, `--no-dev`) instead of hidden envs.
- Prompt interactively when tightening loose ranges; allow `--yes` to auto-
  accept the suggested pins.
- Present structured summaries (counts, version table) for quick scanning.

## Command Proposal

### Detection Matrix

1. If `pyproject.toml` includes `[project]` with dependencies, use it as the
   source of truth. Optional `requirements*.txt` can be imported as dev deps.
2. If `pyproject.toml` is absent but `requirements.txt` exists, scaffold a
   minimal `pyproject.toml` (name inferred from folder) before lock generation.
3. If neither file exists, prompt to run `px project init` (interactive) or
   exit with a hint unless `--init` is provided.

### Flags & Modes

- `--source <path>`: override auto-detected requirements file(s).
- `--dev-source <path>`: specify the dev requirements file.
- `--write` (default interactive confirmation): actually modify files. Without
  `--write`, the command behaves as a dry-run preview.
- `--dry-run`: alias for `--write=never`; exits 0 after preview unless
  conflicts occur.
- `--yes` / `--no-input`: skip interactive prompts (implies `--write`).
- `--lock-only`: update `px.lock` without touching `pyproject.toml`.
- `--allow-dirty`: bypass git status checks (otherwise refuse when the working
  tree is dirty unless only `px.lock` would change).
- `--backup-dir <dir>` (default `.px/onboard-backups`): where snapshots of the
  pre-existing manifests are stored when changes are applied.
- `--json`: emit the envelope described below.

### Workflow

1. Discover inputs (pyproject, requirements files).
2. Parse requirements (support comments, hashes, environment markers) and
   classify into default/dev sets.
3. Resolve versions via the existing resolver pipeline (respecting the
   `PX_ONLINE` gate and any `PX_RESOLVER=0` override); when ranges are found,
   propose pins and require confirmation.
4. Produce a plan table summarizing packages, requested spec, resolved pin,
   source file, and whether it is dev-only.
5. Run safety checks: ensure git worktree clean (unless `--allow-dirty`), and
   that backups can be written.
6. In write mode, create backups, update `pyproject.toml` (optional), and write
   `px.lock` plus a summary of touched files.
7. Print final hints: how to rerun, how to push to CI, pointer to docs.

### Interactive Vs. Non-Interactive

- When stdout is a TTY and `--yes`/`--no-input` absent, show the plan and ask
  “Apply 5 changes and write px.lock? (Y/n)”.
- In non-interactive contexts, require `--write` (or `--yes`) to mutate files;
  otherwise exit with `user-error` instructing the operator to confirm.

### Safety & Backups

- Before writing, `px migrate` copies each file it will touch into
  `.px/onboard-backups/<timestamp>/` unless `--backup-dir` is set to `none`.
- Git check: run `git status --porcelain` and block unless clean or
  `--allow-dirty` is supplied. The error message lists untracked/modified
  files so users can commit or pass the escape hatch.
- File locking: operations use atomic temp files with `fs::rename` to avoid
  partial writes on crash.

### Output Examples

Human (dry-run):

```text
px migrate: plan ready (3 prod, 1 dev, 2 pins resolved)
Hint: rerun with --write to accept pins and produce px.lock

Package    Source            Requested     Resolved     Scope
---------  ----------------  ------------  -----------  -----
flask      requirements.txt  flask>=3.0    flask==3.0.2 prod
rich       requirements.txt  rich          rich==13.7.1 prod
pytest     requirements-dev  pytest==8.2   pytest==8.2  dev
```

JSON (write mode):

```json
{
  "status": "ok",
  "command": "px migrate",
  "details": {
    "actions": {
      "pyproject_updated": true,
      "lock_written": true,
      "backups": [
        ".px/onboard-backups/2025-11-14/pyproject.toml"
      ]
    },
    "packages": [
      {
        "name": "flask",
        "source": "requirements.txt",
        "requested": "flask>=3.0",
        "resolved": "flask==3.0.2",
        "scope": "prod"
      }
    ],
    "git_clean": true,
    "warnings": []
  }
}
```

## Implementation Sketch

- Extend `crates/px-project` with parsers for requirements files (reuse
  `pep508` parser already in `px-resolver`).
- Add a new `OnboardPlan` struct in `px-core` that encapsulates discovery,
  resolution, diffs, and output serialization.
- Wire `px-cli` subcommand to invoke the planner, handle prompts, manage
  backups, and format tables/JSON.
- Reuse existing resolver/store for version selection, with
  `PX_RESOLVER=0` remaining as the escape hatch for legacy installs.

## Test Plan

### Unit

- Requirements parser handles comments/hashes/markers.
- Planner merges pyproject + requirements inputs deterministically.
- Backup module writes to the expected directory and refuses when a previous
  backup exists (unless `--backup-dir` overrides).
- Git guard logic: unit test parsing of `git status --porcelain` output.

### px-cli Integration

- `itest-migrate-pyproject-priority`: pyproject already pins deps; ensure
  requirements import is ignored unless requested.
- `itest-migrate-reqs-basic`: requirements only, successful write.
- `itest-migrate-reqs-loose-vs-pinned`: convert ranges to pins, confirm prompt
  copy and JSON record of tightened specs.
- `itest-migrate-reqs-conflict-detect`: conflicting prod/dev specs raise
  `user-error` with actionable guidance.
- `itest-migrate-dryrun-vs-write`: ensure dry-run touches nothing while write
  produces files and backups.
- `itest-migrate-backup-guard`: refuse when backup dir already contains a
  snapshot unless `--backup-dir` changes.
- `itest-migrate-git-dirty-guard`: dirty worktree abort unless
  `--allow-dirty`.
- `itest-migrate-json-shape`: validate `--json` envelope keys for a happy path.

## Rollout Plan

1. Land the planner + CLI behind `PX_ONBOARD=1` to collect feedback without
   exposing the flag in stable docs.
2. Ship docs/tutorial updates once telemetry or dogfooding confirms UX.
3. Remove the guard, make `px migrate` visible in `px --help`, and announce in
   release notes with migration tips.
4. Follow-ups: add Poetry/uv lock import and richer conflict resolution once
   baseline usage is stable.
