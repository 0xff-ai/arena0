# Rock-Paper-Scissors

`rock-paper-scissors` is a two-participant, best-of-three game. Each participant
answers `ChooseMove` with `"Rock"`, `"Paper"`, or `"Scissors"`.

```console
arena0 run rock-paper-scissors \
  --human host-01 \
  --builtin host-02=sample \
  --replay
```

The program uses commit-reveal so neither choice is public until both
commitments exist. It ends when one participant reaches two wins or after
three rounds, and derives a win or draw with both scores from the final agreed
state.

The receipt proves which choices and scores the participants agreed the
program processed. It does not establish the external identity or custody of
either participant.
