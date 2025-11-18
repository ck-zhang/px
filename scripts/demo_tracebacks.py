#!/usr/bin/env python3
"""
Compare raw Python tracebacks with px-enhanced output.

The script copies the `fixtures/traceback_lab` project into a temporary
directory, syncs it with px, then runs both `python` and `px run` for each
exception you request. It also demonstrates how `px fmt` auto-provisions
its tooling compared to running `python -m ruff format` directly.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Iterable

REPO_ROOT = Path(__file__).resolve().parents[1]
FIXTURE = REPO_ROOT / "fixtures" / "traceback_lab"
DEFAULT_EXCEPTIONS = [
    "ModuleNotFoundError",
    "ImportError",
    "SyntaxError",
    "ValueError",
]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--px-bin",
        default="px",
        help="px binary to invoke (default: %(default)s)",
    )
    parser.add_argument(
        "--python-bin",
        default=sys.executable,
        help="Python interpreter used for the plain runs (default: %(default)s)",
    )
    parser.add_argument(
        "--exceptions",
        nargs="*",
        default=DEFAULT_EXCEPTIONS,
        help="Exception names to exercise (default: %(default)s)",
    )
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        help="Keep the temporary project directory for inspection",
    )
    args = parser.parse_args()

    temp_dir = Path(tempfile.mkdtemp(prefix="px-demo-"))
    project_dir = temp_dir / "traceback_demo"
    shutil.copytree(FIXTURE, project_dir)
    print(f"Demo project: {project_dir}")

    try:
        run_checked([args.px_bin, "sync"], cwd=project_dir)
        for exc in args.exceptions:
            print(f"\n=== {exc} ===")
            run_plain([args.python_bin, "demo_tracebacks.py", exc], cwd=project_dir)
            print()
            run_plain(
                [args.px_bin, "run", "python", "demo_tracebacks.py", exc],
                cwd=project_dir,
            )
            print()

        print("\n=== Formatter demo ===")
        print("python -m ruff format")
        run_plain([args.python_bin, "-m", "ruff", "format"], cwd=project_dir)
        print("\npx fmt")
        run_plain([args.px_bin, "fmt"], cwd=project_dir)
    finally:
        if args.keep_temp:
            print(f"Keeping demo project at {project_dir}")
        else:
            shutil.rmtree(temp_dir, ignore_errors=True)
    return 0


def run_plain(cmd: Iterable[str], cwd: Path) -> None:
    subprocess.run(cmd, cwd=cwd)


def run_checked(cmd: Iterable[str], cwd: Path) -> None:
    subprocess.run(cmd, cwd=cwd, check=True)


if __name__ == "__main__":
    raise SystemExit(main())
