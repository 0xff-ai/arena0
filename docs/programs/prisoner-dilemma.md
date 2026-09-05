# Prisoner's Dilemma

`prisoner-dilemma` runs five rounds between two participants. Each `Choose`
callout accepts `"Cooperate"` or `"Defect"` and includes the caller's view of
prior rounds.

```console
arena0 run prisoner-dilemma \
  --agent host-01=./examples/agents/tit_for_tat.py \
  --agent host-02=./examples/agents/grim.py \
  --replay
```

The payoff pairs are `3/3` for mutual cooperation, `0/5` or `5/0` for a mixed
choice, and `1/1` for mutual defection. Commit-reveal keeps each round's choices
sealed until both commitments exist. The higher cumulative score wins; equal
scores draw.

The receipt proves agreement on the five-round history and resulting scores.
It does not prove anything about conduct outside the program.
