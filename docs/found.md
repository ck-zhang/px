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

## `px init` misreported projects with only px.lock

- Description: Running `px init` in a directory that only contained `px.lock` (no `pyproject.toml`) claimed the project was “already initialized (pyproject.toml present)” instead of guiding the user to restore/create the manifest.
- Repro:
  ```sh
  tmp=$(mktemp -d)
  cd "$tmp"
  touch px.lock
  /home/toxictoast/Documents/0-Code/px/target/debug/px init
  ```
- Expected vs actual: Expected an error about the missing `pyproject.toml` with a hint to restore it or remove the orphaned lock; instead got a misleading “already initialized” message.
- Root cause: `px init` treats the presence of `px.lock` as a fully initialized project and never checks whether `pyproject.toml` actually exists before returning the “already initialized” outcome.
- Fix summary: Detect orphaned lockfiles and return a dedicated user-error pointing out the missing manifest, with remediation guidance; added regression test.

## `px run`/`px test` missing-manifest hint showed the wrong command

- Description: When only `px.lock` was present, the error payload reported `command: "test"` for `px run` and `command: "run"` for `px test`, making the guidance confusing.
- Repro:
  ```sh
  tmp=$(mktemp -d) && cd "$tmp" && touch px.lock
  /home/toxictoast/Documents/0-Code/px/target/debug/px --json run demo | rg command
  /home/toxictoast/Documents/0-Code/px/target/debug/px --json test | rg command
  ```
- Expected vs actual: Expected the `command` detail to match the invoked subcommand; the two were swapped.
- Root cause: The missing-pyproject helper was called with hard-coded command strings copied from the other subcommand.
- Fix summary: Passed the correct command identifiers for run/test and added a regression test.

## Invalid `requires-python` strings looked like missing runtimes

- Description: Projects with an invalid `project.requires-python` (e.g., `not-a-spec`) failed with “python runtime unavailable … no px-managed runtime satisfies …” instead of flagging the bad requirement.
- Repro:
  ```sh
  tmp=$(mktemp -d) && cd "$tmp"
  cat > pyproject.toml <<'EOF'
  [project]
  name = "demo"
  version = "0.1.0"
  requires-python = "not-a-spec"
  dependencies = []
  [tool]
  [tool.px]
  [build-system]
  requires = ["setuptools>=70", "wheel"]
  build-backend = "setuptools.build_meta"
  EOF
  cp /home/toxictoast/Documents/0-Code/px/fixtures/sample_px_app/px.lock .
  /home/toxictoast/Documents/0-Code/px/target/debug/px --json status
  ```
- Expected vs actual: Expected a user error pointing out the invalid Python requirement; instead px claimed no runtime was installed.
- Root cause: `resolve_runtime` treated specifier parse failures as “allowed,” so it fell through to the missing-runtime branch.
- Fix summary: Validate `requires-python` specifiers up front and surface invalid specs in the hint; added CLI coverage.

## Corrupted `.px/state.json` crashed with an eyre backtrace

- Description: If `.px/state.json` contained invalid JSON, `px --json status` printed a color-eyre backtrace instead of structured JSON and guidance.
- Repro:
  ```sh
  tmp=$(mktemp -d) && cd "$tmp"
  /home/toxictoast/Documents/0-Code/px/target/debug/px init >/dev/null
  echo '{not-json' > .px/state.json
  /home/toxictoast/Documents/0-Code/px/target/debug/px --json status
  ```
- Expected vs actual: Expected a user-error telling me the state file was unreadable; instead px crashed with a backtrace and no JSON envelope.
- Root cause: State parsing errors bubbled up as `InstallUserError`, but the CLI dispatcher didn’t translate them into an `ExecutionOutcome`.
- Fix summary: Fail fast on malformed state with a clear hint and map `InstallUserError` to user errors in the dispatcher; added regression coverage.

## `px python info` hid broken pyprojects

- Description: Running `px python info` inside a directory with an invalid `pyproject.toml` ignored the parse failure and reported “no px runtimes registered.”
- Repro:
  ```sh
  tmp=$(mktemp -d) && cd "$tmp"
  printf '[project\nname="broken"\n' > pyproject.toml
  /home/toxictoast/Documents/0-Code/px/target/debug/px --json python info
  ```
- Expected vs actual: Expected the manifest parse error to surface; instead px pretended the project didn’t exist.
- Root cause: `python_info` silently dropped `manifest_snapshot` errors other than “missing project.”
- Fix summary: Propagate manifest errors through to the CLI so they render as user-errors; added coverage.

## Corrupt runtime registries were silently wiped

- Description: A malformed runtime registry (e.g., `PX_RUNTIME_REGISTRY` pointing to `not-json`) caused `px python list` to succeed with an empty set and rewrote the registry, erasing existing runtimes.
- Repro:
  ```sh
  reg=$(mktemp)
  echo 'not-json' > "$reg"
  PX_RUNTIME_REGISTRY="$reg" /home/toxictoast/Documents/0-Code/px/target/debug/px --json python list
  cat "$reg"
  ```
- Expected vs actual: Expected a user error about the bad registry file; instead px returned success, dropped runtimes, and rewrote the file.
- Root cause: The registry loader swallowed JSON parse failures and defaulted to an empty registry, then saved it.
- Fix summary: Parse failures now surface as user-errors with the registry path, and lists no longer clobber the file; tests added.

## Custom registry paths without parent dirs failed `px python install`

- Description: Pointing `PX_RUNTIME_REGISTRY` at a nested path (e.g., `/tmp/runtimes/px/registry.json`) made `px python install --path ...` fail with ENOENT when writing the registry.
- Repro:
  ```sh
  reg=/tmp/runtimes/px/registry.json
  PX_RUNTIME_REGISTRY="$reg" /home/toxictoast/Documents/0-Code/px/target/debug/px python install 3.13 --path "$(which python3)"
  ```
- Expected vs actual: Expected px to create the directory and record the runtime; instead it errored trying to write the registry file.
- Root cause: The registry resolver only created `~/.px` and skipped `create_dir_all` for custom paths.
- Fix summary: Ensure parent directories exist for custom registry locations before reads/writes; covered by a unit test.

## Runtime installs recorded relative interpreter paths

- Description: Installing a runtime with `--path ./py` recorded a relative path; after changing directories, `px python list` pruned the runtime as “missing.”
- Repro:
  ```sh
  tmp=$(mktemp -d) && cd "$tmp"
  ln -s "$(which python3)" py
  PX_RUNTIME_REGISTRY="$tmp/reg.json" /home/toxictoast/Documents/0-Code/px/target/debug/px python install 3.13 --path ./py
  cd /
  PX_RUNTIME_REGISTRY="$tmp/reg.json" /home/toxictoast/Documents/0-Code/px/target/debug/px --json python list
  ```
- Expected vs actual: Expected the registry to store an absolute interpreter path; instead the relative entry was dropped once the cwd changed.
- Root cause: `install_runtime` stored the provided path verbatim without canonicalizing it.
- Fix summary: Canonicalize explicit interpreter paths before inspection so registry entries remain valid across directories; unit test added.

## External runtimes were ignored during resolution

- Description: Recording a runtime with `PX_RUNTIME_REGISTRY` that pointed to an existing system Python (outside `~/.px`) still produced “no px-managed runtime satisfies …” when running project commands.
- Repro:
  ```sh
  reg=$(mktemp)
  cat > "$reg" <<'EOF'
  {"runtimes":[{"version":"3.13","full_version":"3.13.7","path":"/usr/bin/python3","default":true}]}
  EOF
  tmp=$(mktemp -d) && cd "$tmp"
  cp /home/toxictoast/Documents/0-Code/px/fixtures/sample_px_app/pyproject.toml .
  cp /home/toxictoast/Documents/0-Code/px/fixtures/sample_px_app/px.lock .
  PX_RUNTIME_REGISTRY="$reg" /home/toxictoast/Documents/0-Code/px/target/debug/px --json status
  ```
- Expected vs actual: Expected px to use the recorded interpreter; instead it errored saying no px-managed runtime was installed.
- Root cause: `resolve_runtime` filtered out any runtime not under the px-managed runtimes directory when satisfying requirements.
- Fix summary: Prefer px-managed runtimes but fall back to external entries when they satisfy the requirement; added regression coverage.

## `px python install` errors produced backtraces

- Description: Passing an invalid interpreter path to `px python install` rendered a color-eyre backtrace instead of a user-facing error.
- Repro:
  ```sh
  /home/toxictoast/Documents/0-Code/px/target/debug/px python install 3.11 --path /does/not/exist
  ```
- Expected vs actual: Expected a user-error explaining the bad path; instead px crashed with a backtrace.
- Root cause: The python subcommands returned raw errors to the dispatcher, which only mapped missing-project/manifest cases.
- Fix summary: Wrap python registry/install failures into `ExecutionOutcome` user-errors and teach the dispatcher to render `InstallUserError` values cleanly.

## `PX_ONLINE=no/off` was treated as online

- Description: Setting `PX_ONLINE` to common falsey values like `no` or `off` still left px in “online” mode, so commands that should skip network access behaved as if the network was available.
- Repro:
  ```sh
  PX_ONLINE=no /home/toxictoast/Documents/0-Code/px/target/debug/px --json migrate --apply 2>/dev/null
  # px still attempts online behavior instead of treating PX_ONLINE as disabled
  ```
- Expected vs actual: Expected `PX_ONLINE=no|off|0|false` to disable network; instead only `0`/`false` were honored.
- Root cause: Env parsing only checked for `0`/`false`, ignoring other common falsey strings and the empty value.
- Fix summary: Normalize the flag parser to treat `0/false/no/off/""` as offline; added a unit test for the matrix.
