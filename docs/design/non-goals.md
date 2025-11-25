# Non-goals

px does **not**:

* Act as a general task runner (no `px task` DSL).
* Manage non-Python languages.
* Provide a plugin marketplace or unbounded extension API.
* Implicitly mutate state from read-only commands (`status`, `why`, `fmt` with respect to project/workspace state).
* Expose `cache` or `env` as primary user concepts.

Workspaces are an advanced concept for multi-project repos; most users can treat px as a per-project tool. If future changes violate these, they’re design regressions, not “nice additions”.
