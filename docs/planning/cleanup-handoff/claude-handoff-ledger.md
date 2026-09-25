# Claude handoff ledger and continuation proposal

Recorded 2026-09-25. Source: @thread:thr_k94vjvhz6a. Successor: @thread:thr_jsrdc8mjrq.

Current status: D3b integrated, milestone review resolved, restored projection tests integrated, recorded audit/diagnostic follow-ups completed, and full verification passed. Final HEAD `fe3b066`. Historical inspection snapshots below remain for provenance; the completion entry at the end is authoritative. Do not restart D3b or replay the old queued continuation without reading this ledger.

The initial handoff phase was inspection and planning. The user then instructed: "checkpoint this work first (commit), then continue". Execution resumed under that instruction; the appended entries below supersede the initial snapshot where noted.

## Established user decisions

- Finish the whole task autonomously once executing; the parent drives scope, API shape, contracts, integration, and acceptance. Workers implement a specified shape and escalate obstacles instead of inventing APIs. Source user events: 1325, 1361, 1762, 2499, 3277.
- Audit the whole branch against `main`, including exported shape, duplicate concepts, and ownerless types. The earlier requested method was `ast-grep outline`.
- Compile modules once and reuse a stable Wasmtime cache across runs. This was an explicit requirement at event 472, not an optional optimization.
- Implementation/testing/chores: Muse Spark 1.3 contributor, medium reasoning, meta through Pi. Reuse worker context where appropriate (event 555).
- Latest review rule overrides the earlier Sol rule: Muse Spark 1.3, max reasoning, meta; reviews only at major milestones (event 5186). Do not silently revert this because Codex is available again.
- Use the delegate skill for any future worker operation. The parent remains accountable for reviewing actual diffs and evidence.
- Branch is `raulk/fix-dual-arch`; task ledger says **never push**. No external publication is authorized by this proposal.
- Preserve existing `.bb/` and `.serena/` content and the configured compiler wrapper/cache. Do not loosen timeouts or tests to make gates green.

## Verified handoff state

- Both source and successor threads use environment `env_je3uk7ygnr`, checkout `/data/arena0`.
- Checkout HEAD: `92f2fb74ee849c2c168eabd75a47052a7075faff`. Tracked files are clean; `.bb/` and `.serena/` are untracked directories.
- D3b worker: @thread:thr_k5qbnvnywi.
- D3b worktree: `/home/ubuntu/.bb-machines/raul.getbb.app/plugins/environment-git-worktree/host-data/worktrees/thr_k5qbnvnywi-1/arena0`.
- Worktree HEAD is the same `92f2fb7`. D3b remains a mixture of staged deletions, unstaged edits, and one untracked test. Read `git diff HEAD`, not only `git diff`, when accepting it.
- Tracked delta: 33 files, 626 insertions, 4,811 deletions. These numbers exclude the untracked `crates/arena0-sdk/tests/data_macro.rs`, which must be included in integration.
- Tracked binary diff SHA-256 at inspection: `30b77538ea7bcd04792badeb62412c9268bebe2b130abe46ffc798262639a4ca`.
- Untracked `data_macro.rs` SHA-256: `885b9831058d225b0e4aace3045722f19a38552da4eaaa8d2f4e8e4e0e09d2ba`.
- Source thread was in error with no active background agents when inspected. It has system retry `qmsg_bmuwk2v6tw` scheduled for 2026-09-25 18:00:36.923 UTC. Recheck and establish a single active driver before implementation. No queue or process was changed in this turn.
- An earlier duplicate-driver incident was a BB bug (user events 3728/3766). Do not assume a second driver is unrelated work or kill a process based only on its name.

## What already landed

The source thread inherited earlier stages from @thread:thr_6b9is45iux. Its latest task ledger records these completed Stage D units:

| Unit | Result | Commit |
|---|---|---|
| D2a | One pass through dispatch | `b7552d4` |
| D2d | Plain-argument sandbox calls | `c3294a2` |
| D2e | One context type parameterized by handler mode | `f52574a` |
| Follow-up | Collapse nested dispatch commit check | `cb983ae` |
| D2c | Store keeps each terminal/local-state fact once | `a4459a3` |
| D2b | Bounded Borsh module and derived codecs | `92f2fb7` |

D1 and D3a were also completed earlier in the source history. D1 consolidated Host identity and removed dead code. D3a addressed compile/cache/setup costs. Their reports remain in the predecessor's `impl/` storage; milestone review should verify the resulting requirements, not repeat these units.

D2b had a parent diff review and passing gate, but no independent model review under the revised milestone-only rule. The milestone review must include it. Recorded D2b gate: 745 host tests and 58 program tests, plus build/check/doc-tests. A later import placement fix received focused formatting/lint verification.

## D3b intended shape and last parent edits

D3b deletes the native SDK execution harness and moves behavioral scenarios into existing real-Host Arena tests. Pure program logic stays in native tests. Native host-import shims fail explicitly instead of simulating effects. Harness-only divergence diagnostics and the `arena0::test` macro are removed.

The implementation report is **not the final state**. Claude made these changes after reading it:

1. Restored `public_data_macro_compiles_explicit_generic_contract_bounds` in `crates/arena0-sdk/tests/data_macro.rs`. It never depended on the deleted harness and could not be dropped under the contract.
2. Added `programs/chess/src/lib.rs::draw_rules_classify_terminal_boards`, covering stalemate, the fifty-move rule, and insufficient material through the actual `Status::compute` logic.
3. Removed `Run::query`: no workspace program advertises a query and no scenario used it. This is a parent-approved amendment to the original contract, which listed the helper.
4. Removed the generated, untracked minimal-example lockfile.

Confirmed final helper surface includes `Run::callout`, `Run::view`, and `Expect::respond_rejected`; `Run::query` is absent. Claude accepted the small chess test constant/helper that the mechanical contract checker flagged. Reconcile these rulings with the contract/report before acceptance instead of restoring obsolete contract details.

### Coverage exceptions still needing explicit disposition

- Minimal example: `Shared.phase` is `ManagedPhase<Phase>`, so the contract's direct phase initializer did not compile. Worker used `..Default::default()`; reported two example tests pass.
- Terminal views: worker reported `ExecCommand::View` cannot reach an execution actor after terminal shutdown. Existing scenarios assert callouts, mid-game views, outcomes, and receipts, but those do not establish final-view behavior.
- Claude provisionally accepted several harness-related test removals, while flagging final-view access as a possible product gap. Do not silently turn that provisional acceptance into proof that all old observable promises remain covered.
- Before landing, finish the deleted-test mapping and identify any lost pure rendering/rule assertion that can be preserved without reviving the harness. A protocol/runtime behavior change requires checking the owning architecture and stating any disagreement first.
- Keep the distinction between removing tests of deleted harness machinery and dropping product-rule coverage. Counts alone are not an acceptance argument.

## Verification evidence

Claude launched the final gate in the D3b worktree immediately before the rate limit:

```sh
cargo fmt --all --check && just build-programs && just check && time just test
```

The retained log `/data/arena0/.bb/d3b-gate.log` ends in `exit=0`. It completed after the source thread stopped:

- 727/727 host tests passed, zero skipped.
- SDK/primitives doctests succeeded, with documented ignored examples and six compile-fail cases passing.
- 22/22 program tests passed, zero skipped.
- `just test` wall time: 1m13.21s.

This gate includes Claude's restored tests and query-helper deletion. The handoff inspection independently ran `git diff HEAD --check` in the worktree; it passed. It did not rerun the suite. The hashes above describe current inspected content; they were not recorded by the original gate itself.

Earlier worker evidence, not rerun here: Arena-only 39/39 in 26.965s; minimal example 2/2; an earlier full test pass in 1m55.686s. A subsequent worker run hit a daemon MCP timeout that passed alone. Prefer the final parent's gate for the patch as it stands.

Reuse successful checks while relevant inputs are unchanged. If acceptance or integration changes code, choose the focused proof for those changes and run the required aggregate gate once after stabilization. Rebuild guests after SDK/macro changes. Do not mistake the background shell's exit code for the gate's result: the explicit `exit=0` in the log is the relevant evidence.

## Proposed continuation sequence

1. **Establish one driver and freeze the baseline.** Recheck source retry/worker state and worktree hashes before mutation. Preserve the existing patch, untracked macro test, and logs. Resolve any resumed Claude activity before either driver edits shared files.
2. **Close D3b acceptance.** Read the final diff against the original contract plus the parent amendments above. Correct the stale coverage table, settle terminal-view/pure-rendering coverage, check removed symbols and documentation, and record each accepted deviation. Do not re-add unused `Run::query` simply to satisfy the old contract text.
3. **Integrate D3b locally.** Include the untracked test. Check actual HEAD and diff before the Conventional Commit. Exclude handoff artifacts and unrelated local files. Reuse the existing successful gate only if its relevant inputs remain unchanged; otherwise run the affected gate after stabilization. No push.
4. **Run the Stage D milestone review.** Use Muse Spark 1.3 max via delegate, over the complete branch against `main`, with the parent owning review rulings. Explicit targets: public API shape, durable fact ownership, D2b encoding compatibility, real-Host test coverage, compile-once/cache requirements, and architecture agreement. The parent also completes the final structural audit. The existing audit is evidence, not a substitute for inspecting D3b's final additions.
5. **Resolve the recorded follow-ups.** Diagnose daemon startup/MCP timeouts from existing failure logs before reproducing. Decide where bounded codecs belong after tracing wire/program dependencies. Narrow `resolve_test_cache_dir` visibility if confirmed internal. Profile the same agreed-step path before selecting a performance fix; the old ~100ms estimate is not a current bottleneck measurement. Keep each change scoped and separately justified.
6. **Close the whole task.** Update the authoritative Stage D task ledger and this decision ledger; record review disposition, final commit IDs, verification inputs/results, and any explicitly deferred product issue. A green D3b patch alone does not finish the branch work.

### Design choices deferred until evidence is collected

The wire crate has its own bounded helpers while `arena0-program::bounded` now owns similar codecs. Do not blindly add a wire-to-program dependency or create another abstraction. Inspect the actual ownership/dependency boundary, choose one owner, and specify the caller migration before delegating.

The daemon failures occurred on both base and changed trees in prior A/B runs. This argues against blaming D2c/D2b, but it does not establish the root cause. Start with `.bb/daemon-ab.log`, `.bb/d2b-gate.log`, and the worker failures. Do not increase deadlines or rerun unchanged suites until a hypothesis warrants it.

## Decisions made by the successor in this turn

| ID | Decision | Reason / evidence |
|---|---|---|
| H001 | Inspection and proposal only; preserve implementation state | Latest explicit request asks to study and propose |
| H002 | Treat final worktree plus parent gate as newer than worker report | Source events 5486–5552 and current files |
| H003 | Include the untracked macro test in any future integration | Tracked diff alone omits a restored coverage promise |
| H004 | Preserve query-helper deletion as a parent contract amendment | Source event 5517; no callers or advertised query program |
| H005 | Require explicit coverage disposition for terminal views | Successful outcomes/receipts do not prove terminal rendering |
| H006 | Reuse gate evidence; no redundant suite during handoff | Final log passed; this turn made no source changes |
| H007 | Check automatic retry before implementation | Source queue can resume another driver on the same checkout |
| H008 | Preserve latest model/review policy and no-push boundary | User event 5186 and authoritative task ledger |

Append future entries with decision, reason, evidence, implementation/commit, and verification. Distinguish a proposal from an accepted decision and a worker claim from independently inspected evidence.

## Evidence index

- Full retrieved source history: [claude-source-thread.json](/data/arena0/.bb/handoff/claude-source-thread.json), 5,005 events through sequence 5564.
- Original D3b contract: [D3b.md](contract-d3b.md).
- Authoritative tasks: [tasks-stage-d.md](tasks-stage-d.md).
- Earlier branch shape audit: [audit-shape.md](audit-shape.md).
- Worker report before parent fixes: [d3b-report.txt](/data/arena0/.bb/d3b-report.txt).
- Final parent gate: [d3b-gate.log](/data/arena0/.bb/d3b-gate.log).
- Prior dropped-test inspection: [d3b-dropped.txt](/data/arena0/.bb/d3b-dropped.txt).

Tool note: context-mode subprocesses lacked BB connection environment variables. Native `bb` successfully retrieved the history. `ctx_execute_file` cannot read thread storage outside the project, so a transcript copy was placed under `.bb/handoff/` for analysis. No service configuration was changed. Ripwire was used for a focused structural query; no graphify graph existed and none was built for this handoff.

## Execution resumed: checkpoint

- H009: User explicitly authorized checkpointing and continuation. Committed the unchanged D3b patch, including the previously untracked macro test, as `ae94a88` (`refactor(test): run program scenarios through real hosts`). Read actual HEAD and diff first; staged whitespace check passed. Fast-forwarded `raulk/fix-dual-arch` to that commit. No push, history rewrite, or source modification.
- The commit contains 34 files, 651 insertions, and 4,811 deletions. Tracked checkout is clean. `.bb/` and `.serena/` remain untracked; raw transcripts and local orchestration state were not included in the source commit.
- Reused Claude's final gate because the committed source is the same inspected patch. This checkpoint is not a claim that milestone review or coverage follow-ups are complete.
- H010: Milestone review uses a separate delegate unit with a fixed base `5caeea9e4a4fb4ea6e9edbfae72e82ded881c891` (current local `main`) and checkpoint `ae94a88`. Review role was verified as Pi / `meta/muse-spark-1.3-contributor`, max reasoning. Existing D3b implementer is referenced for provenance; no implementation turn is requested from it.
- Milestone reviewer started: @thread:thr_eeyfe3mv4w, delegate unit `StageD-milestone`, review round 1. On completion, run `delegate wait-any`, inspect its report and cited code, rule on each finding, then continue the parent's audit and follow-ups. The delegate workflow requires yielding while the worker runs; completion wakes this thread.

## Milestone review rulings

- H011: Accept missing terminal projection assertions for RPS, chess and contract-net. Restore pure ProgramView/Program outcome tests with fixed fixtures, without reviving a harness or widening production visibility. A bounded three-file worker contract specifies exact test symbols and assertions.
- Reject the vickrey portion of Finding 1: `programs/vickrey-auction/src/lib.rs::view_fills_all_slots_and_mono_has_no_escape_sequences` already asserts `Final revealed bids`, `Sold to P1 for 80`, and `complete | sold to P1 for 80` for three color depths. No duplicate test warranted.
- H012: Reject Finding 2's claim that no daemon test pins post-completion submit. `crates/arena0-tests/tests/daemon_api.rs::competing_callout_submissions_return_typed_conflict_and_execution_continues` drives both sides to completion, retries the old request, and checks `CalloutNotPending` (lines 216–221 at checkpoint). Server::submit reads durable callout state before resolving a live driver. Arena's retired-actor panic is not the agent-facing behavior.
- H013: Confirm Finding 4 resolved: terminal daemon views use persisted shared state and already have a real daemon journey. Do not add runtime behavior for an Arena limitation.
- H014: Preserve separate bounded codecs: wire owns canonical transport frames with wire-specific errors and program owns program Borsh fields; adding wire→program would invert the intended boundary. No new shared abstraction is justified by these helpers alone.
- Finding 3 remains under parent assessment: actual newest/oldest selection differs, but the claimed two-distinct-callout restart scenario must be checked against retention/announcement behavior before treating it as a demonstrated bug.
- `D3b-projections` started on Muse medium, worker @thread:thr_t7mqaxm5mw, base `ae94a88`. Only its three program test files are authorized; parent retains Arena, cache visibility, timeout diagnosis and performance/audit follow-ups. On completion inspect actual diff and run the focused/aggregate checks only as inputs require.

## Integrated milestone fixes and diagnostic results

- H015: Projection tests integrated as `ffb13fc` (`test(programs): restore terminal projection coverage`). Parent reviewed all 348 added lines. Tightened RPS assertions to associate each participant with its own score/hand on the same line; broad whole-slot substring checks could pass swapped labels. Focused RPS test passed after that edit. Worker evidence for the unchanged chess/contract-net tests: 8/8 and 3/3 respectively; all three packages passed before the RPS tightening. No production visibility change was needed; chess's test module nests inside its owner.
- H016: Align `Expect::submit` with `Run::callout` by selecting the newest callout announcement. This is a consistency fix, not a claim that the reviewer's hypothetical restart race was reproduced. Existing rejection/retry and multi-round real-Host scenarios exercise the path; no synthetic test of iterator structure was added.
- H017: Narrow `resolve_test_cache_dir` to `pub(crate)` after confirming all callers are in its crate. Replace its link in public documentation with a description so rustdoc does not link public API to a private helper. Engine/cache behavior is unchanged.
- H018: The saved daemon failure log shows six Host-open deadline failures and an MCP hello timeout during heavily parallel workspace tests. The existing `kind(test)` nextest group did not include these library tests. Change the group filter to `kind(test) | package(arena0-daemon)` so daemon library tests share the existing four-test real-Host budget. Keep all deadlines, assertions and test inclusion unchanged. `cargo nextest show-config test-groups -p arena0-daemon --no-pager` confirms the intended membership; log: `.bb/handoff/daemon-test-groups.log`. This addresses a demonstrated scheduling gap; a passing run alone cannot establish that every intermittent timeout has the same root cause.
- H019: Keep wire/program bounded helpers separate after dependency and error-contract inspection. Wire has only crypto/Borsh/error dependencies and owns frame admission with wire-specific errors; program owns guest-field codecs. No dependency inversion or new lowest-common-denominator crate was introduced.
- H020: Parent ran `ast-grep outline --stdin --lang rust --json=compact` over 32 Rust files changed since Claude's audited `92f2fb7`. Comparing public outline entries found only `ObservedCallout` plus its two fields, `Run::callout`, `Run::view`, `Expect::respond_rejected`, and the reduced cache-helper visibility; 213 old public entries removed. Combine this delta inspection with the existing parent main-to-D2b audit and completed whole-branch milestone review. Outline is structural evidence, not macro-expansion or behavioral proof.

### Step-path timing follow-up

Used a temporary test in the existing RPS test module, calling its actual `completed_run` journey three times and verifying receipts. Attached the existing recording tracing subscriber and used `Run::progress_timeline`; no production instrumentation or runtime hooks were added. Temporary test was removed before the final gate. A first attempt to include the module from a separate test failed on Rust inner documentation comments; corrected by placing the diagnostic in the original module. Retained log: `.bb/handoff/step-profile.log`.

| Trial | Terminal publication | Participant 0 median certified-step interval | Participant 1 median |
|---|---|---|---|
| 0 | 2.212 s | 88.53 ms | 99.89 ms |
| 1 | 1.634–1.645 s | 87.84 ms | 89.92 ms |
| 2 | 1.637 s | 82.92 ms | 99.90 ms |

Each participant certified nine steps. Largest observed interval was 176.73 ms. These are event-observation intervals, including scenario input/polling and transport/consensus, not isolated handler CPU times.

The run recorded 216 SQLite transaction commits across three two-participant journeys: median 5.31 ms, maximum 18.87 ms, summed 1.296 s across participants. Three `program_load` records: first 554.523 ms (`compiled`); subsequent loads about 0.05 ms. Cache directory was `/data/arena0/target/wasmtime-cache`. This confirms in-process reuse on the measured path and quantifies one contributor; it does not prove SQLite dominates the critical path or independently test cross-process cache hits. Stable persistent-cache wiring was inspected in milestone review. No speculative optimization, profile setting change, or weakened durability followed from these measurements.

### Final verification in progress

`just check-affected` selected `full` but stopped in its untracked-file whitespace scan on old `.bb/*.diff` artifacts (and the newly written log). No Rust check ran in that attempt. Preserved those local artifacts and ran the selected equivalent directly: `just check test`, with retained output `.bb/handoff/final-direct-gate.log`. Tracked `git diff --check` passed. Do not fix or discard historical evidence merely to satisfy the untracked scan.

- H021: Full guest rebuild exposed D3b's unused `arena0_program::abi::imports` on wasm32; every use is inside a native-only shim. Added the same native cfg to that import only. No executable behavior changes. Because the full gate had already compiled its host binaries before this one-line cleanup, verify the cleanup with a subsequent guest rebuild and SDK all-target Clippy; retain the full gate for behavior and state this sequencing explicitly.

## Completion record — 2026-09-25

Final local branch HEAD: `fe3b066` on `raulk/fix-dual-arch`. No push. Tracked working tree clean after commits; pre-existing `.bb/` and `.serena/` remain local/untracked. Ledger and diagnostic artifacts intentionally remain local; they are not public-repository content.

| Commit | Result |
|---|---|
| `ae94a88` | Checkpoint and integrate Claude's final D3b patch |
| `ffb13fc` | Restore RPS, chess and contract-net terminal rendering/outcome assertions |
| `d1fd32a` | Include daemon library tests in existing concurrency budget; align Arena callout selection |
| `7505f41` | Keep test cache-path helper internal |
| `fe3b066` | Scope the native diagnostic import to native targets |

`just check test` exited 0: dependency checks, engine-version test, both workspace format checks and Clippy, guest build, **727/727 host tests**, SDK/primitives doctests, and **25/25 program tests**. No skipped nextest tests. Full host suite wall time 80.408 s. Existing ignored documentation examples remain as before. Full output: `.bb/handoff/final-direct-gate.log`.

After the one-line import cfg cleanup: formatting, `just build-programs`, and `cargo clippy --locked -p arena0-sdk --all-targets` exited 0 with no warnings. All ten generated Wasm hashes equal those from the successful full gate. Output: `.bb/handoff/import-cleanup-build.log`. No need to repeat unchanged behavioral checks for removal of an unused import.

Delegate units `D3b`, `D3b-projections`, and `StageD-milestone` are marked approved/done. The parent accepted the terminal projection fixes after reviewing actual code; no extra per-unit model review was added, consistent with the user's milestone-only rule. The authoritative predecessor task ledger is updated with these rulings and results.

### Remaining limits for Claude

- The earlier source thread's automatic retry was still queued for **2026-09-25 18:00:36.923 UTC** when last inspected. Its queue was not changed. Any resumed source driver must first reconcile with this final HEAD and ledger; its old task/worker reports are stale.
- `just check-affected` on the dirty checkout includes untracked historical `.bb` artifacts and fails their whitespace. The equivalent selected full gate passed directly. No check logic was weakened and no saved artifacts were edited to hide this.
- Timing measurements identify end-to-end intervals and SQLite commit costs, not a CPU flamegraph or a proved optimization opportunity. Cross-process persistent cache wiring was reviewed, while the measured run established in-process reuse. No performance improvement is claimed beyond the existing cached loading path.
- The concurrency fix closes a concrete scheduling omission and passed the full suite. It is not a proof that unrelated machine-load failures cannot recur.
- No release/publish gate, package upload, PR, push, or new runtime behavior was part of this continuation.

## Original-plan reconciliation

On the user's question about what remains, re-read predecessor `impl/design.md`, the predecessor conversation's user decisions and stage completion reports, and actual `main..HEAD` commit history. The agreed implementation stages are all accounted for:

| Stage | Agreed result | Evidence |
|---|---|---|
| A | Architecture/proposal revised first | `d4a845c` plus later architecture decisions |
| B2 | Delete async callout lowering, alternate trait form, labels, notifications | `9e3a079` |
| B5 | One timer event/payload | `ea42a21` |
| B1 | Bad input rejects without ending the session | `cbbd932` |
| B3 | Synchronous guest signing | `7b5cd7d` |
| B4 | Callouts derived from state, accepted answer consumes its ID | `0c1720d`, `7d03152` |
| B6 | Divergence terminates instead of wedging execution | `5b6adae` |
| T | Certified final step owns terminal evidence | `3a904a1` |
| C1/C2 | Plain store; actor owns live execution state | `c418992` |
| C3 | State-driven delivery; explicit Open/Ending/Ended | `ee5bef2` |
| E | Validate effects on emission, apply on accepted return; remove React/deferred broadcast | `bf49bcf` |
| D | Identity, codecs, dispatch/context/store consolidation, cache reuse, real-Host test migration | D-stage commits through `fe3b066` above |

No agreed implementation stage remains open. This is a reconciliation of existing evidence, not a fresh line-by-line acceptance review of every earlier change. Milestone review, final structural audit, restored coverage and full checks are complete.

Separate follow-ups: identifying and optimizing the remaining ~100 ms step cost would require deeper profiling; current timings do not prove the root cause. Intermittent test stability can be monitored after the scheduling correction; one green run is not a universal flake-freedom guarantee. Release/package checks and publishing were not required by the cleanup plan and have not been performed. The original branch instruction remains never push. The source-thread automatic retry remains an operational handoff item, not unfinished implementation.

## Remote branch authorization — 2026-09-25

- H022: User explicitly requested: "Push this to a branch on the remote, including planning docs and ledgers". This supersedes the earlier no-push instruction for this publication.
- Publish the completed code plus an archived planning bundle to `origin`, branch `raulk/arena0-cleanup-handoff`. The existing remote branch was checked and did not exist. Do not force-push or modify main.
- Bundle path: `docs/planning/cleanup-handoff/`. Include the original design, stage designs/contracts, structural audit, decisions, storage/survey notes, completed task ledger, milestone review contract/report, projection-test contract and this handoff ledger. Preserve historical decisions as records; the latest ledger rulings supersede earlier proposals and review claims.
- Raw provider transcripts, build logs, copied source audit trees, delegate runtime state, `.serena/`, and local AGENTS.md remain outside the publication. Selected documents were checked for credential-like literals; none were found. Local evidence paths remain historical references, not bundled artifacts.
