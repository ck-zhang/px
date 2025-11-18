# px Traceback Demo

Use `scripts/demo_tracebacks.py` to compare raw Python behavior with px. This
script copies the `fixtures/traceback_lab` project into a temporary directory,
syncs it with `px`, and then runs both `python` and `px run` for a list of
exceptions. It also demonstrates how `px fmt` auto-installs its tooling.

```bash
python scripts/demo_tracebacks.py --exceptions ModuleNotFoundError SyntaxError
```

Options:

* `--px-bin` – override which `px` binary to call (defaults to `px` on `PATH`).
* `--python-bin` – override the Python interpreter used for the non-px runs.
* `--keep-temp` – keep the generated demo project instead of deleting it.

Each section prints two command invocations so you can capture or stream the
outputs side-by-side in demos.
