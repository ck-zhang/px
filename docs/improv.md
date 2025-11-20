# UX Improvements to Consider

- Clarify and improve default `px run` argument forwarding. Right now `px run -- ARG` treats `ARG` as an entry instead of passing it to the inferred target; letting `--` forward to the default entry (or surfacing a hint) would better match the core workflow described in docs/spec.md.
- The default `px fmt` runner errors until users manually add Ruff. Auto-installing the default formatter (or prompting with a one-shot installer) would smooth the first-run experience without diverging from the expectations in docs/spec.md.
- Tool installs can sporadically fail with “invalid JSON” while fetching PyPI metadata. Adding a lightweight retry/backoff around the release fetch would make installs more resilient and keep behavior consistent with docs/spec.md’s reliability goals.
