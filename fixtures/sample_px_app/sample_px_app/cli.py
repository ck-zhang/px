"""Deterministic greeting CLI used by px integration tests."""

from __future__ import annotations

import argparse
import sys

from rich.console import Console


_CONSOLE = Console(color_system=None, highlight=False, markup=False)


def greet(name: str = "World") -> str:
    """Return a deterministic greeting for the provided name."""

    return f"Hello, {name}!"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Print a greeting.")
    parser.add_argument("-n", "--name", default="World", help="Name to greet")
    args = parser.parse_args(argv)

    _CONSOLE.print(greet(args.name))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
