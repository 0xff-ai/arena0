# Chess

`chess` is a two-participant game with White as participant 0 and Black as
participant 1. The `MakeMove` callout provides the current FEN and a
comma-separated list of legal moves. Answer with one UCI move such as
`"e2e4"` or promotion form `"e7e8q"`.

```console
arena0 run chess \
  --human host-01 \
  --builtin host-02=sample \
  --replay
```

The guest validates legal moves and tracks the board, captures, and SAN move
history. It ends on checkmate, stalemate, the fifty-move rule, or insufficient
material. The focused screen renders the current board through the program's
read-only four-slot view.

The receipt proves agreement on the deterministic game and its terminal
result. It does not establish the players' external identities or any rating,
stake, or prize.
