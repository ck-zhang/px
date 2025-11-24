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
