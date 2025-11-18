"""
Raise specific Python exceptions or warnings to exercise px traceback UX.

Usage:

    python demo_tracebacks.py ModuleNotFoundError

Pass --list to print all supported exception names.
"""

from __future__ import annotations

import argparse
import builtins
import sys
import warnings


EXCEPTION_NAMES = [
    "BaseException",
    "BaseExceptionGroup",
    "Exception",
    "GeneratorExit",
    "KeyboardInterrupt",
    "SystemExit",
    "ArithmeticError",
    "AssertionError",
    "AttributeError",
    "BufferError",
    "EOFError",
    "ImportError",
    "LookupError",
    "MemoryError",
    "NameError",
    "OSError",
    "ReferenceError",
    "RuntimeError",
    "StopAsyncIteration",
    "StopIteration",
    "SyntaxError",
    "SystemError",
    "TypeError",
    "ValueError",
    "Warning",
    "FloatingPointError",
    "OverflowError",
    "ZeroDivisionError",
    "BytesWarning",
    "DeprecationWarning",
    "EncodingWarning",
    "FutureWarning",
    "ImportWarning",
    "PendingDeprecationWarning",
    "ResourceWarning",
    "RuntimeWarning",
    "SyntaxWarning",
    "UnicodeWarning",
    "UserWarning",
    "BlockingIOError",
    "ChildProcessError",
    "ConnectionError",
    "FileExistsError",
    "FileNotFoundError",
    "InterruptedError",
    "IsADirectoryError",
    "NotADirectoryError",
    "PermissionError",
    "ProcessLookupError",
    "TimeoutError",
    "IndentationError",
    "_IncompleteInputError",
    "IndexError",
    "KeyError",
    "ModuleNotFoundError",
    "NotImplementedError",
    "PythonFinalizationError",
    "RecursionError",
    "UnboundLocalError",
    "UnicodeError",
    "BrokenPipeError",
    "ConnectionAbortedError",
    "ConnectionRefusedError",
    "ConnectionResetError",
    "TabError",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
    "UnicodeTranslateError",
    "ExceptionGroup",
]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("exception", nargs="?", help="Exception class name to raise")
    parser.add_argument(
        "--list",
        action="store_true",
        help="List available exceptions and exit",
    )
    args = parser.parse_args()
    if args.list or not args.exception:
        for name in EXCEPTION_NAMES:
            print(name)
        return 0
    try:
        raise_named_exception(args.exception)
    except Exception as exc:  # pragma: no cover - demonstration helper
        raise exc
    return 0


def raise_named_exception(name: str) -> None:
    exc_type = getattr(builtins, name, None)
    if exc_type is None:
        raise ValueError(f"unknown exception '{name}'")

    special_handler = SPECIAL_CASES.get(name)
    if special_handler is not None:
        special_handler(exc_type)
        return

    if issubclass(exc_type, Warning):
        warnings.simplefilter("error", exc_type)
        warnings.warn(f"demo warning for {name}", exc_type)
        return

    raise exc_type(f"demonstration {name}")


def _raise_missing_module(_: type[BaseException]) -> None:
    raise ModuleNotFoundError("No module named 'demo_missing'")


def _raise_import_error(_: type[BaseException]) -> None:
    raise ImportError("No module named 'demo_missing'")


def _raise_exception_group(exc_type: type[BaseException]) -> None:
    inner = ValueError("inner error")
    raise exc_type("demo exception group", [inner])


def _raise_unicode_decode_error(exc_type: type[BaseException]) -> None:
    raise exc_type("utf-8", b"\xff", 0, 1, "invalid start byte")


def _raise_unicode_encode_error(exc_type: type[BaseException]) -> None:
    raise exc_type("utf-8", "demo", 0, 1, "cannot encode demo")


def _raise_unicode_translate_error(exc_type: type[BaseException]) -> None:
    raise exc_type("demo", 0, 1, "cannot translate demo")


def _raise_system_exit(exc_type: type[BaseException]) -> None:
    raise exc_type(42)


SPECIAL_CASES: dict[str, callable[[type[BaseException]], None]] = {
    "ModuleNotFoundError": _raise_missing_module,
    "ImportError": _raise_import_error,
    "ExceptionGroup": _raise_exception_group,
    "BaseExceptionGroup": _raise_exception_group,
    "UnicodeDecodeError": _raise_unicode_decode_error,
    "UnicodeEncodeError": _raise_unicode_encode_error,
    "UnicodeTranslateError": _raise_unicode_translate_error,
    "SystemExit": _raise_system_exit,
}


if __name__ == "__main__":
    sys.exit(main())
