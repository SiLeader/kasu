#!/usr/bin/env python3
"""Example oracle: require a byte sequence in a file, using Metadata only."""
import argparse
import os
import sys

from kasu_bot import BotError, Budget, Client, Rejected, wire


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    path = parser.add_mutually_exclusive_group(required=True)
    path.add_argument("--path")
    path.add_argument("--path-hex")
    expected = parser.add_mutually_exclusive_group(required=True)
    expected.add_argument("--contains")
    expected.add_argument("--contains-hex")
    args = parser.parse_args()
    try:
        path = args.path.encode() if args.path is not None else bytes.fromhex(args.path_hex)
        required = args.contains.encode() if args.contains is not None else bytes.fromhex(args.contains_hex)
        if not path or not required:
            return 2
        client = Client(os.environ["KASU_METADATA_ENDPOINT"], os.environ["KASU_REPO"], os.environ.get("KASU_TOKEN_FILE"), Budget(20))
        response = client.call("read-file", id=wire(os.environ["KASU_CHANGESET"]), path=list(path))
        return 0 if required in bytes(response["content"]) else 1
    except Rejected as error:
        return 1 if error.status == 404 else 2
    except (BotError, KeyError, ValueError, TypeError, OSError):
        return 2


if __name__ == "__main__":
    sys.exit(main())
