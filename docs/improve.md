# Product testing follow-ups

- Set `[project].requires-python` default to `>=3.11` per spec, so `px init` no longer copies the host interpreter version.
- Keep pins stable: re-running `px add foo` when `foo` is already pinned no longer loosens the requirement.
- Tool installs now fall back to the system Python if the requested channel matches, avoiding forced `px python install` when a suitable interpreter exists.
- `px fmt` runs in strict (read-only) mode so it won’t resolve or rewrite locks/runtimes implicitly.
- Progress spinners disable themselves on non-TTY stderr to avoid log flooding in CI or captured output.
- Default `px run` entry inference now skips the implicit `<package>.cli` fallback unless the module exists, so users get a clear “no entry configured” outcome instead of a ModuleNotFound error.
- Runtime downloads respect `PX_RUNTIME_REGISTRY` by placing runtimes alongside the registry file instead of always using `~/.px/runtimes`.
