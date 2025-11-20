import builtins
import sys


def build_exception(name: str) -> BaseException:
    cls = getattr(builtins, name)
    if name in {"BaseExceptionGroup", "ExceptionGroup"}:
        return cls("demo", [ValueError("inner failure")])
    if name == "SystemExit":
        return cls(1)
    if name == "KeyboardInterrupt":
        return cls()
    if name == "GeneratorExit":
        return cls()
    if name == "ModuleNotFoundError":
        return cls("No module named 'demo_missing'")
    if name == "ImportError":
        return cls("No module named 'demo_missing'")
    if name == "UnicodeDecodeError":
        return cls("utf-8", b"bad", 0, 1, "decode issue")
    if name == "UnicodeEncodeError":
        return cls("utf-8", "bad", 0, 1, "encode issue")
    if name == "UnicodeTranslateError":
        return cls("bad", 0, 1, "translate issue")
    if name == "StopIteration":
        return cls("stop")
    if name == "StopAsyncIteration":
        return cls("stop")
    if name == "_IncompleteInputError":
        # Follows the SyntaxError signature
        return cls("incomplete input", ("<stdin>", 1, 1, ""))
    return cls(f"demo {name}")


def main(argv: list[str] | None = None) -> None:
    args = argv or sys.argv[1:]
    target = args[0] if args else "Exception"
    if not hasattr(builtins, target):
        raise SystemExit(f"unknown exception: {target}")
    raise build_exception(target)


if __name__ == "__main__":
    main()
