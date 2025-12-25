#!/usr/bin/env python3
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
from __future__ import annotations

import os
import platform
import sys
from pathlib import Path


def read_os_release_pretty_name() -> str | None:
    path = Path("/etc/os-release")
    try:
        data = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    for line in data.splitlines():
        if line.startswith("PRETTY_NAME="):
            value = line.split("=", 1)[1].strip()
            return value.strip('"')
    return None


def main() -> None:
    sandbox = os.environ.get("PX_SANDBOX", "0")
    sandbox_id = os.environ.get("PX_SANDBOX_ID", "")
    pretty = read_os_release_pretty_name()

    print("Hello from px run-by-reference")
    print(f"python {platform.python_version()} ({sys.executable})")
    print(f"sandbox={sandbox}" + (f" id={sandbox_id}" if sandbox_id else ""))
    print(f"kernel {platform.system()} {platform.release()} ({platform.machine()})")
    if pretty:
        print(f"os-release {pretty}")
    else:
        print("os-release <unavailable>")


if __name__ == "__main__":
    main()
