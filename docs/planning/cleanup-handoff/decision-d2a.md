# D2a conflict decision (S4 vs failed_persist test)

Option 1. Update the test to the new contract. Keep the S4 code as you applied it.

In `crates/arena0-node/src/execution/tests.rs`:
- Rename `failed_persist_reloads_state_and_restores_resident_before_next_dispatch`
  to `failed_persist_reloads_state_and_rebuilds_resident_on_next_dispatch`.
- Replace the three lines
  `let resident = actor.instance.as_ref().unwrap().committed_payloads();` and the two
  `assert_eq!(resident.…)` lines with:
  ```rust
  assert!(
      actor.instance.is_none(),
      "a failed persist drops the resident; the next use rebuilds it"
  );
  ```
- Keep everything else. After the second `dispatch_event` returns `Committed`, add:
  ```rust
  let resident = actor.instance.as_ref().expect("rebuilt resident").committed_payloads();
  assert_eq!(resident.0, actor.state.shared_state());
  assert_eq!(resident.1, actor.state.local_state());
  ```
  just before the existing `local_state() == [9]` assertion.

No other change. Then run the remaining gates from fix2-d2a.md
(`cargo test -p arena0-protocol -p arena0-store -p arena0-node -p arena0-sandbox`,
then `just test`) and write `impl/report-d2a-fix2.md` as specified there.
