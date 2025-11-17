"""Helper entry that intentionally references a missing module."""

import imaginary_package  # type: ignore[import-not-found]


def main() -> None:
    raise SystemExit(imaginary_package)  # pragma: no cover


if __name__ == "__main__":
    main()
