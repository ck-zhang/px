def main():
    if os.environ.get("ALL_PROXY"):
        # Emulate a proxy-induced socks dependency error without touching the network.
        raise RuntimeError("Missing dependencies for SOCKS support.")
    print("ok")


if __name__ == "__main__":
    raise SystemExit(main())
