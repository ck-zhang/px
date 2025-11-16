# sample_px_app

Minimal deterministic package used to exercise px flows (`init → add → run → test`).
It pins [`rich==13.7.1`](https://github.com/Textualize/rich) so px’s install
and publish steps always touch at least one third-party wheel.

## Try it with px

1. `cd fixtures/sample_px_app`.
2. `cargo run -q -- install` (or `px sync`) to build `px.lock` and fetch
   `rich`.
3. `cargo run -q -- run sample-px-app -- -n Demo` to see the Rich-powered
   greeting.
4. `cargo run -q -- test` (optionally `PX_TEST_FALLBACK_STD=1`) to run the CLI
   smoke test.
