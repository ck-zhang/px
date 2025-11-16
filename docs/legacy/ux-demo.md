# UX Demo & Checklist

## 1. Quick Demo (≈2 min)

```bash
# sample app walkthrough
cd fixtures/sample_px_app
px project init                # clean init (no flags)
px project add requests==2.32.3
px project remove requests
px sync && px tidy
px lock diff && px lock upgrade
px run -- -n Demo              # default entry inference
px run python -- -m sample_px_app.cli -n Demo
px test                        # emits px workflow test: …
px cache path && px cache stats
px cache prune --all --dry-run
px store prefetch --dry-run    # PX_ONLINE=1 optional for dry-run
PX_ONLINE=1 px store prefetch
px workspace list
px workspace sync && px workspace verify --json
PX_SKIP_TESTS=1 px build --json
PX_ONLINE=1 PX_PUBLISH_TOKEN=… px publish --dry-run --json

# real-project passthrough
cd /home/toxictoast/test/black
px run python src/black/brackets.py
```

Tips:

- Set `PX_ONLINE=1` before networked commands (prefetch/publish/install smoke).
- `PX_PUBLISH_TOKEN` must be populated for real publish (dry-run works offline).

## 2. Acceptance Checklist

Each row matches `px <group> <command>` with expected copy.

| Command (human) | Expected line | JSON sketch |
| --- | --- | --- |
| `px project init` | `px project init: initialized project demo`<br>`Hint: Pass --package …` when inferred | `{status:"ok",message:"px project init: …",details:{package:"demo",files_created:[…]}}` |
| `px project add` | `px project add: updated dependencies (added X, updated Y)` | `{details:{added:["requests"],updated:[]}}` |
| `px project remove` | `px project remove: removed dependencies` | `{details:{removed:["requests"]}}` |
| `px run -- -n X` | `px workflow run: Hello, X!` | `{details:{mode:"module",entry:"sample_px_app.cli"}}` |
| `px run python -- -m pkg.cli` | same greeting | `{details:{mode:"passthrough",program:"python"}}` |
| `px test` | `px workflow test: pytest …` (or fallback) | `{details:{runner:"pytest"|"builtin"}}` |
| `px infra env python` | `px infra env: /usr/bin/python3` | `{details:{mode:"python",interpreter:"…"}}` |
| `px infra env info` | `px infra env: interpreter … • project …` | `{details:{mode:"info",project_root:"…"}}` |
| `px infra env paths` | `px infra env: pythonpath entries: N` | `{details:{mode:"paths",paths:["…"]}}` |
| `px infra cache path` | `px infra cache: path /abs/cache` | `{details:{status:"path",path:"…"}}` |
| `px infra cache stats` | `px infra cache: stats: X files, Y bytes` | `{details:{status:"stats",total_entries:X}}` |
| `px infra cache prune --all --dry-run` | `px infra cache: would remove N files (size)` | `{details:{status:"dry-run"}}` |
| `px store prefetch --dry-run` | `px store prefetch: dry-run N artifacts (M cached)` | `{details:{status:"dry-run",summary:{requested:N,hit:M}}}` |
| `px store prefetch` | `px store prefetch: hydrated N artifacts (M cached, K fetched)` | `{details:{status:"prefetched"}}` |
| `px store prefetch` gated | `px store prefetch: PX_ONLINE=1 required for downloads`<br>`Hint: export PX_ONLINE=1 or add --dry-run to inspect work without downloading` | `{status:"user-error",details:{status:"gated-offline"}}` |
| `px workspace list` | `px workspace list: member-alpha, member-beta` | `{details:{workspace:{members:[…]}}}` |
| `px workspace sync` | `px workspace sync: all N members clean` or drift summary | `{details:{workspace:{counts:{ok,drifted,failed}}}}` |
| `px workspace verify` | `px workspace verify: all N members clean` or `drift in member-X …` + Hint | `{details:{status:"clean|drift"}}` |
| `px workspace tidy` | `px workspace tidy: all N members clean` | `{details:{workspace:{members:[…]}}}` |
| `px quality tidy` | `px quality tidy: px.lock matches pyproject` | `{details:{status:"clean"}}` |
| `px lock diff` | `px lock diff: clean` or `px lock diff: drift (…)` + Hint | `{details:{status:"clean|drift",added:[…]}}` |
| `px lock upgrade` | `px lock upgrade: upgraded lock to version 2` | `{details:{status:"upgraded",version:2}}` |
| `px cache path/stats/prune` | lines above | JSON with `status:"path|stats|dry-run"` |
| `px store` workspace | `px store prefetch: workspace dry-run …` | `{details:{workspace:{totals:{…}}}}` |
| `PX_SKIP_TESTS=1 px build --json` | `px build: wrote N artifacts (size, sha256=XYZ…)` | `{details:{artifacts:[…],format:"both",skip_tests:"1"}}` |
| `px publish --dry-run` | `px publish: dry-run to pypi (N artifacts)` | `{details:{registry:"pypi",dry_run:true}}` |
| `px publish` gating | `px publish: PX_ONLINE=1 required for uploads`<br>`Hint: export PX_ONLINE=1 && PX_PUBLISH_TOKEN=… before publishing` | `{status:"user-error",details:{registry:"pypi",token_env:"PX_PUBLISH_TOKEN"}}` |
| `px run python src/black/brackets.py` | surfaces real upstream error | `{details:{mode:"passthrough",program:"python"}}` |

Use this sheet to confirm each command prints one status line, at most one
Hint, and a predictable JSON envelope before demos or release reviews.

### Delivery examples

**`px build`**

```bash
$ PX_SKIP_TESTS=1 px build
px build: wrote 2 artifacts (755 B, sha256=bc77…)
```

```json
{
  "status": "ok",
  "message": "px build: wrote 2 artifacts (755 B, sha256=bc77…)",
  "details": {
    "artifacts": [
      {
        "path": "build/sample-px-app-0.1.0.tar.gz",
        "bytes": 755,
        "sha256": "bc77dd37…"
      }
    ],
    "format": "both",
    "skip_tests": "1"
  }
}
```

**`px publish` dry-run**

```bash
$ px publish --dry-run
px publish: dry-run to pypi (2 artifacts)
```

```json
{
  "status": "ok",
  "message": "px publish: dry-run to pypi (2 artifacts)",
  "details": {
    "registry": "pypi",
    "dry_run": true,
    "token_env": "PX_PUBLISH_TOKEN"
  }
}
```

**`px publish` gating**

```bash
$ px publish
px publish: PX_ONLINE=1 required for uploads
Hint: export PX_ONLINE=1 && PX_PUBLISH_TOKEN=<token> before publishing
```

```json
{
  "status": "user-error",
  "message": "px publish: PX_ONLINE=1 required for uploads",
  "details": {
    "registry": "pypi",
    "token_env": "PX_PUBLISH_TOKEN",
    "hint": "export PX_ONLINE=1 && PX_PUBLISH_TOKEN=<token> before publishing"
  }
}
```
