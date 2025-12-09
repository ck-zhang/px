# Non-goals

px does **not**:

* Act as a general task runner (no `px task` DSL).
* Manage non-Python languages.
* Support native Windows hosts (use WSL; we aim for Unix-first).
* Provide a plugin marketplace or unbounded extension API.
* Implicitly mutate state from read-only commands (`status`, `why`, `fmt` with respect to project/workspace state).
* Expose `cache` or `env` as primary user concepts.
* Act as a general frontend for OS package managers or conda environments. px
  may use apt/apk/dnf/conda-forge internally in sandbox and builder images, but
  those providers are not part of the user model; system dependencies are
  expressed only via `[tool.px.sandbox]` capabilities.

Workspaces are an advanced concept for multi-project repos; most users can treat px as a per-project tool. If future changes violate these, they’re design regressions, not “nice additions”.
