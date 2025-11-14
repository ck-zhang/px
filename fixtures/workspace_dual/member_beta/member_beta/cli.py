def greet(name: str | None = None) -> str:
    target = name or "Workspace"
    return f"Hello from beta, {target}!"


def main() -> None:
    print(greet())
