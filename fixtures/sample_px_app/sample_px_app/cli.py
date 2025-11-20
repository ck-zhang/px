from __future__ import annotations

import argparse


def greet(name: str = "World") -> str:
    return f"Hello, {name}!"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="sample-px-app")
    parser.add_argument("-n", "--name", dest="name", default=None)
    parser.add_argument("positional_name", nargs="?", default=None)
    args = parser.parse_args(argv)
    target = args.positional_name or args.name or "World"
    print(greet(target))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
