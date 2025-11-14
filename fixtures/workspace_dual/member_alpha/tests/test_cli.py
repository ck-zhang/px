from member_alpha import cli  # type: ignore[import-not-found]


def test_greet_default() -> None:
    assert cli.greet() == "Hello from alpha, Workspace!"
