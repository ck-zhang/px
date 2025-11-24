## `px init --py` mangles operators

- Description: Passing an explicit operator like `~=3.11` to `px init --py` wrote an invalid `requires-python = ">=~=3.11"` to `pyproject.toml`, breaking consumers that parse the requirement.
- Repro:
  ```sh
  tmp=$(mktemp -d)
  cd "$tmp"
  /home/toxictoast/Documents/0-Code/px/target/debug/px init --py "~=3.11"
  grep requires-python pyproject.toml
  ```
- Expected vs actual: Expected `requires-python = "~=3.11"`; saw `requires-python = ">=~=3.11"`.
- Root cause: `resolve_python_requirement_arg` blindly prefixed `>=` to any value not starting with `>`, so other valid comparison operators were corrupted.
- Fix summary: Detect any leading comparison operator and preserve the user’s specifier, only prefixing `>=` for bare version numbers. Added a CLI test to lock in the behavior.

## Dry-run flags performed real mutations

- Description: `--dry-run` on mutating commands (e.g., `px init --dry-run` or `px add --dry-run requests`) still created/modifed project files and even kicked off resolution work.
- Repro:
  ```sh
  tmp=$(mktemp -d)
  cd "$tmp"
  /home/toxictoast/Documents/0-Code/px/target/debug/px init --dry-run
  ls  # pyproject.toml and px.lock were created

  /home/toxictoast/Documents/0-Code/px/target/debug/px init
  cp pyproject.toml before.toml
  cp px.lock lock.before
  /home/toxictoast/Documents/0-Code/px/target/debug/px add requests --dry-run
  diff -u before.toml pyproject.toml  # shows real edits
  diff -u lock.before px.lock
  ```
- Expected vs actual: Expected a no-op preview that left the working tree untouched; instead the commands modified `pyproject.toml`/`px.lock` and could trigger installs.
- Root cause: The shared `--dry-run` flag was parsed but never handled in the core logic for init/add/remove/update/sync, so execution proceeded normally.
- Fix summary: Added dry-run handling to project init/add/remove/update/sync with backups/preview outputs and no writes, and covered with CLI tests.

## Missing-lock hint for `px sync --frozen` was circular

- Description: Running `px sync --frozen` without a lockfile printed “Run `px sync` before `px sync`,” which is confusing guidance.
- Repro:
  ```sh
  tmp=$(mktemp -d)
  cd "$tmp"
  /home/toxictoast/Documents/0-Code/px/target/debug/px init
  rm px.lock
  /home/toxictoast/Documents/0-Code/px/target/debug/px --json sync --frozen
  ```
- Expected vs actual: Expected a hint to generate the lockfile with a non-frozen sync; instead the hint repeated the same command.
- Root cause: The missing-lock diagnostic used a generic hint string for all commands, so `sync` produced a self-referential message.
- Fix summary: Tailored the missing-lock hint for `px sync` to direct users to run a non-frozen sync, and added a regression test.

## Broken pyproject/px.lock crashed with backtraces

- Description: If `pyproject.toml` or `px.lock` contained invalid TOML (or `pyproject.toml` was missing while `px.lock` existed), commands failed with an eyre backtrace pointing into `dispatch.rs` instead of a user-actionable error.
- Repro:
  ```sh
  tmp=$(mktemp -d)
  cd "$tmp"
  printf '[project\nname="broken"\n' > pyproject.toml
  /home/toxictoast/Documents/0-Code/px/target/debug/px --json status

  tmp=$(mktemp -d)
  cd "$tmp"
  cat > pyproject.toml <<'EOF'
  [project]
  name = "demo"
  version = "0.1.0"
  requires-python = ">=3.11"
  dependencies = []
  [tool]
  [tool.px]
  [build-system]
  requires = ["setuptools>=70", "wheel"]
  build-backend = "setuptools.build_meta"
  EOF
  echo "not toml" > px.lock
  /home/toxictoast/Documents/0-Code/px/target/debug/px --json status
  ```
- Expected vs actual: Expected a clear user-error pointing to the bad file with a hint to fix/regenerate; instead px crashed with a backtrace.
- Root cause: TOML parse errors and missing-manifest errors bubbled to the CLI as uncaught anyhow errors with no friendly mapping.
- Fix summary: Added contextual error handling for manifest/lock parsing and missing manifest detection, converting them into `user-error` outcomes with actionable hints; added tests to lock the behavior.

## No-op dry-run/force flags leaked into run/test/fmt

- Description: `px run`, `px test`, and `px fmt` accepted `--dry-run`/`--force` in their help/CLI even though the flags were ignored, making it look like you could preview commands without running them.
- Repro:
  ```sh
  /home/toxictoast/Documents/0-Code/px/target/debug/px run --dry-run
  ```
- Expected vs actual: Expected the flags to be rejected (or to actually skip execution); instead they were silently accepted then the command proceeded normally.
- Root cause: A shared flag struct was flattened into read-only commands even though the options weren't implemented there.
- Fix summary: Removed the mutation-only flags from run/test/fmt so unsupported options are rejected early; added a regression test to ensure the parser stops on the unused flag.

## `px sync --dry-run` hid resolver errors

- Description: Dry-run sync reported success even when dependency resolution would fail, e.g., with an invalid requirement string.
- Repro:
  ```sh
  tmp=$(mktemp -d)
  cd "$tmp"
  cat > pyproject.toml <<'EOF'
  [project]
  name = "demo"
  version = "0.1.0"
  requires-python = ">=3.11"
  dependencies = ["not a spec"]
  [tool]
  [tool.px]
  [build-system]
  requires = ["setuptools>=70", "wheel"]
  build-backend = "setuptools.build_meta"
  EOF
  /home/toxictoast/Documents/0-Code/px/target/debug/px --json sync --dry-run
  ```
- Expected vs actual: Expected a user-error about the bad requirement; instead the command claimed it would resolve and write the lockfile.
- Root cause: The dry-run path only looked at state flags and never attempted resolution, so resolver failures were masked.
- Fix summary: Dry-run sync now runs the resolver without writing files and surfaces the same `resolve_failed` user-error; test added to prevent regressions.
