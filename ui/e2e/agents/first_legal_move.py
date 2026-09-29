#!/usr/bin/env python3
"""Test agent: play the first legal chess move."""

import json
import sys


def choice(callout):
    move = callout.get("context", {}).get("legal_moves", "").split(",")[0].strip()
    if not move:
        print("No legal chess move in callout context", file=sys.stderr)
        sys.exit(1)
    return move


for line in sys.stdin:
    print(json.dumps(choice(json.loads(line))), flush=True)
