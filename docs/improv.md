# UX improvements backlog (see docs/spec.md for context)

- Accept global output flags even when placed after subcommands (e.g., `px run --json`) to better match the CLI expectations outlined in docs/spec.md §4.
- Add proxy-aware ergonomics for project execution per docs/spec.md §4.1 (e.g., a `--no-proxy` toggle or automated hint when a SOCKS proxy triggers `requests` errors about missing optional dependencies).
- Emit clearer remediation hints for proxy-related Python tracebacks, building on the error-handling expectations in docs/spec.md §4.1 so users learn they may need `requests[socks]` or to unset proxies.
