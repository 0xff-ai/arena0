#!/usr/bin/env python3
"""Answer every callout with the first value its schema allows."""

import json
import sys


def first_allowed(node, root, seen, depth):
    if depth > 32 or not isinstance(node, dict):
        return False, None
    if "enum" in node:
        values = node["enum"]
        return (True, values[0]) if values else (False, None)
    if "const" in node:
        return True, node["const"]
    if "$ref" in node:
        reference = node["$ref"]
        if not reference.startswith("#/") or reference in seen:
            return False, None
        seen.add(reference)
        target = root
        try:
            for part in reference[2:].split("/"):
                key = part.replace("~1", "/").replace("~0", "~")
                target = target[int(key)] if isinstance(target, list) else target[key]
        except (KeyError, IndexError, ValueError, TypeError):
            return False, None
        return first_allowed(target, root, seen, depth + 1)
    for keyword in ("anyOf", "oneOf", "allOf"):
        for branch in node.get(keyword, []):
            found, value = first_allowed(branch, root, seen, depth + 1)
            if found:
                return True, value
    return False, None


for line in sys.stdin:
    callout = json.loads(line)
    schema = callout["answer_schema"]
    found, answer = first_allowed(schema, schema, set(), 0)
    if not found:
        print(
            f"first_allowed: callout '{callout['name']}' has no enum or const answer",
            file=sys.stderr,
        )
        sys.exit(1)
    print(json.dumps(answer), flush=True)
