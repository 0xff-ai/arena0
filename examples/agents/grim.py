#!/usr/bin/env python3
"""Cooperate until the opponent defects, then defect for the rest of the run."""

import json
import sys


punishing = False
for line in sys.stdin:
    callout = json.loads(line)
    allowed = callout["answer_schema"]["enum"]
    cooperate = next(value for value in allowed if value.lower() == "cooperate")
    defect = next(value for value in allowed if value.lower() == "defect")
    history = callout.get("context", {}).get("history", "")
    punishing = punishing or "them=defect" in history.lower()
    print(json.dumps(defect if punishing else cooperate), flush=True)
