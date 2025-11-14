def greet(name: str | None = None) -> str:
    target = name or "Workspace"
    return f"Hello from alpha, {target}!"


def main() -> None:
    print(greet())
