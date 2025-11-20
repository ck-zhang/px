Implemented UX fixes (aligned with docs/spec.md):

- `px tool install` now rejects requirement-like names (e.g., `ruff==0.6.9`) with a hint to pass the name and spec separately.
- `px fmt` accepts `--json` on the subcommand (in addition to the global flag) for structured output.
- `px fmt` success/error details include the tool runtime and tool root to expose the active tool environment.
