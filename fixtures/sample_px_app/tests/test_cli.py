from sample_px_app import cli  # type: ignore[import-not-found]


def test_greet_default() -> None:
    assert cli.greet() == "Hello, World!"


def test_greet_with_name() -> None:
    assert cli.greet("Test") == "Hello, Test!"
