#!/usr/bin/env python3
"""Cooperate first, then copy the opponent's latest Prisoner's Dilemma move."""

import json
import sys


def choice(callout):
    allowed = callout["answer_schema"]["enum"]
    cooperate = next(value for value in allowed if value.lower() == "cooperate")
    defect = next(value for value in allowed if value.lower() == "defect")
    history = callout.get("context", {}).get("history", "")
    if not history:
        return cooperate
    latest_round = history.rstrip().rsplit("R", 1)[-1]
    return defect if "them=defect" in latest_round.lower() else cooperate


for line in sys.stdin:
    print(json.dumps(choice(json.loads(line))), flush=True)
