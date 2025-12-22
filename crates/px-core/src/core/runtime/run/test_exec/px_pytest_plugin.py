import os
import sys
import time
import pytest
from _pytest._io.terminalwriter import TerminalWriter


class PxTerminalReporter:
    def __init__(self, config):
        self.config = config
        self.is_tty = sys.stdout.isatty()
        self._tw = TerminalWriter(file=sys.stdout)
        self._tw.hasmarkup = self.is_tty
        self.session_start = time.time()
        self.collection_start = None
        self.collection_duration = 0.0
        self.collected = 0
        self.files = []
        self._current_file = None
        self.failures = []
        self.collection_errors = []
        self.stats = {
            "passed": 0,
            "failed": 0,
            "skipped": 0,
            "error": 0,
            "xfailed": 0,
            "xpassed": 0,
        }
        self.exitstatus = 0
        self.spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        self.spinner_index = 0
        self.last_progress_len = 0
        self._spinner_active = self.is_tty

    def pytest_sessionstart(self, session):
        import platform

        py_ver = platform.python_version()
        root = str(self.config.rootpath)
        cfg = self.config.inifile or "auto-detected"
        self._tw.line(
            f"px test  •  Python {py_ver}  •  pytest {pytest.__version__}",
            cyan=True,
            bold=True,
        )
        self._tw.line(f"root:   {root}")
        self._tw.line(f"config: {cfg}")
        self.collection_start = time.time()
        self._render_progress(note="collecting", force=True)

    def pytest_collection_finish(self, session):
        self.collected = len(session.items)
        files = {str(item.fspath) for item in session.items}
        self.files = sorted(files)
        self.collection_duration = time.time() - (
            self.collection_start or self.session_start
        )
        label = "tests" if self.collected != 1 else "test"
        file_label = "files" if len(self.files) != 1 else "file"
        self._spinner_active = False
        self._clear_spinner(newline=True)
        self._tw.line(
            f"collected {self.collected} {label} from {len(self.files)} {file_label} in {self.collection_duration:.2f}s"
        )
        self._tw.line("")

    def pytest_collectreport(self, report):
        if report.failed:
            self.stats["error"] += 1
            summary = getattr(report, "longreprtext", "") or getattr(
                report, "longrepr", ""
            )
            self.collection_errors.append((str(report.fspath), str(summary)))
        self._render_progress(note="collecting")

    def pytest_runtest_logreport(self, report):
        if report.when not in ("setup", "call", "teardown"):
            return
        status = None
        if report.passed and report.when == "call":
            status = "passed"
            self.stats["passed"] += 1
        elif report.skipped:
            status = "skipped"
            self.stats["skipped"] += 1
        elif report.failed:
            status = "failed" if report.when == "call" else "error"
            self.stats[status] += 1

        if status:
            file_path = str(report.location[0])
            name = report.location[2]
            duration = getattr(report, "duration", 0.0)
            self._print_test_result(file_path, name, status, duration)

        if report.failed:
            self.failures.append(report)

    def pytest_sessionfinish(self, session, exitstatus):
        self.exitstatus = exitstatus
        self._spinner_active = False
        self._clear_spinner(newline=True)
        if self.failures:
            self._render_failures()
        if self.collection_errors:
            self._render_collection_errors()
        self._render_summary(exitstatus)

    # --- rendering helpers ---
    def _render_progress(self, note="", force=False):
        if not self.is_tty:
            return
        if not force and not self._spinner_active:
            return
        total = self.collected or "?"
        completed = (
            self.stats["passed"]
            + self.stats["failed"]
            + self.stats["skipped"]
            + self.stats["error"]
            + self.stats["xfailed"]
            + self.stats["xpassed"]
        )
        frame = self.spinner_frames[self.spinner_index % len(self.spinner_frames)]
        self.spinner_index += 1
        line = f"\r{frame} {completed}/{total} • pass:{self.stats['passed']} fail:{self.stats['failed']} skip:{self.stats['skipped']} err:{self.stats['error']}"
        if note:
            line += f" • {note}"
        padding = max(self.last_progress_len - len(line), 0)
        sys.stdout.write(line + (" " * padding))
        sys.stdout.flush()
        self.last_progress_len = len(line)

    def _clear_spinner(self, newline: bool = False):
        if not self.is_tty:
            return
        if self.last_progress_len:
            end = "\n" if newline else "\r"
            sys.stdout.write("\r" + " " * self.last_progress_len + end)
            sys.stdout.flush()
            self.last_progress_len = 0

    def _print_test_result(self, file_path, name, status, duration):
        if self._current_file != file_path:
            self._current_file = file_path
            self._tw.line("")
            self._tw.line(file_path)
        icon, color = self._status_icon(status)
        dur = f"{duration:.2f}s"
        line = f"  {icon} {name}  {dur}"
        self._tw.line(line, **color)

    def _render_failures(self):
        self._tw.line(f"FAILURES ({len(self.failures)})", red=True, bold=True)
        self._tw.line("-" * 11)
        for idx, report in enumerate(self.failures, start=1):
            self._render_single_failure(idx, report)

    def _render_collection_errors(self):
        self._tw.line(
            f"COLLECTION ERRORS ({len(self.collection_errors)})", red=True, bold=True
        )
        self._tw.line("-" * 19)
        for idx, (path, summary) in enumerate(self.collection_errors, start=1):
            self._tw.line("")
            self._tw.line(f"{idx}) {path}", bold=True)
            if summary:
                for line in str(summary).splitlines():
                    self._tw.line(f"   {line}", red=True)

    def _render_single_failure(self, idx, report):
        path, lineno = self._failure_lineno(report)
        self._tw.line("")
        self._tw.line(f"{idx}) {report.nodeid}", bold=True)
        self._tw.line("")
        message = self._failure_message(report)
        if message:
            self._tw.line(f"   {message}", red=True)
            self._tw.line("")
        snippet = self._load_snippet(path, lineno)
        if snippet:
            file_line = f"   {path}:{lineno}"
            self._tw.line(file_line)
            for i, text in snippet:
                pointer = "→" if i == lineno else " "
                self._tw.line(f"  {pointer}{i:>4}  {text}")
            self._tw.line("")
        explanation = self._assertion_explanation(report)
        if explanation:
            self._tw.line("   Explanation:")
            for line in explanation:
                self._tw.line(f"     {line}")

    def _render_summary(self, exitstatus):
        total = sum(self.stats.values())
        duration = time.time() - self.session_start
        status_label = "✓ PASSED" if exitstatus == 0 else "✗ FAILED"
        status_color = {"green": exitstatus == 0, "red": exitstatus != 0, "bold": True}
        self._tw.line("")
        self._tw.line(
            f"RESULT   {status_label} (exit code {exitstatus})", **status_color
        )
        self._tw.line(f"TOTAL    {total} tests in {duration:.2f}s")
        self._tw.line(f"PASSED   {self.stats['passed']}")
        self._tw.line(f"FAILED   {self.stats['failed']}")
        self._tw.line(f"SKIPPED  {self.stats['skipped']}")
        self._tw.line(f"ERRORS   {self.stats['error']}")

    # --- utility helpers ---
    def _status_icon(self, status):
        if status in ("passed", "xpassed"):
            return "✓", {"green": True}
        if status in ("skipped", "xfailed"):
            return "∙", {"yellow": True}
        return "✗", {"red": True, "bold": True}

    def _failure_message(self, report):
        longrepr = getattr(report, "longrepr", None)
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            return longrepr.reprcrash.message
        if hasattr(report, "longreprtext"):
            return report.longreprtext.splitlines()[0]
        return str(longrepr) if longrepr else "test failed"

    def _load_snippet(self, path, lineno, context=2):
        path = str(path)
        try:
            with open(path, "r", encoding="utf-8") as f:
                lines = f.readlines()
        except OSError:
            return None
        start = max(0, lineno - context - 1)
        end = min(len(lines), lineno + context)
        snippet = []
        for idx in range(start, end):
            text = lines[idx].rstrip("\n")
            snippet.append((idx + 1, text))
        return snippet

    def _failure_lineno(self, report):
        longrepr = getattr(report, "longrepr", None)
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            return str(longrepr.reprcrash.path), longrepr.reprcrash.lineno
        path, lineno, _ = report.location
        return str(path), lineno + 1

    def _assertion_explanation(self, report):
        longrepr = getattr(report, "longrepr", None)
        summary = None
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            summary = longrepr.reprcrash.message or ""
        if summary:
            lowered = summary.lower()
            if "did not raise" in lowered:
                expected = summary.split("DID NOT RAISE")[-1].strip()
                expected = expected or "expected exception"
                summary = f"Expected {expected} to be raised, but none was."
            elif "assert" in lowered and "==" in summary:
                parts = summary.split("==", 1)
                left = (
                    parts[0]
                    .replace("AssertionError:", "")
                    .replace("assert", "", 1)
                    .strip()
                )
                right = parts[1].strip()
                summary = f"Expected: {right}"
                if left:
                    summary += f"\n     Actual:   {left}"
            else:
                summary = summary.replace("AssertionError:", "").strip()
        if not summary:
            return None
        parts = summary.split("\n")
        return [part for part in parts if part.strip()]


def pytest_configure(config):
    config.option.color = "yes" if sys.stdout.isatty() else "no"
    pm = config.pluginmanager
    reporter = PxTerminalReporter(config)
    default = pm.getplugin("terminalreporter")
    if default:
        pm.unregister(default)
        pm.register(reporter, "terminalreporter")
        config._px_reporter_registered = True
    else:
        config._px_reporter_registered = False
    config._px_reporter = reporter


def pytest_sessionstart(session):
    config = session.config
    reporter = getattr(config, "_px_reporter", None)
    if reporter is None:
        return
    if not getattr(config, "_px_reporter_registered", False):
        pm = config.pluginmanager
        default = pm.getplugin("terminalreporter")
        if default and default is not reporter:
            pm.unregister(default)
        pm.register(reporter, "terminalreporter")
        config._px_reporter_registered = True
    reporter.pytest_sessionstart(session)
