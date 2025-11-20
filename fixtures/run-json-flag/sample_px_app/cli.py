import argparse

def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("name", nargs="?", default="World")
    args = parser.parse_args(argv)
    print(f"Hello, {args.name}!")

if __name__ == "__main__":
    raise SystemExit(main())
