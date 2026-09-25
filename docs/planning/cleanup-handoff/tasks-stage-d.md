# Stage D task list (branch raulk/fix-dual-arch, never push)

- [x] D2a one pass through dispatch — b7552d4
- [x] D2d plain-argument sandbox calls — c3294a2
- [x] D2e one Ctx type by handler mode — f52574a
- [x] clippy collapsible-if from D2a — cb983ae
- [x] D2c store keeps each fact once — a4459a3
- [x] D2b bounded Borsh module + derived codecs — 92f2fb7 (own diff read; no model review per 2026-09-25 rule: reviews only at major milestones, Muse Spark 1.3 max)
- [x] D3b delete native SDK harness; Arena test API — checkpoint ae94a88, integrated by successor thr_jsrdc8mjrq; final parent gate passed 727 host / 22 program tests. Coverage exceptions remain explicit milestone review targets.
- [x] Stage D milestone review (Muse Spark 1.3 max), thr_eeyfe3mv4w — parent verified/rule on findings; projection gaps restored in ffb13fc; post-terminal submit and vickrey assertions already existed. Full gate passed. See /data/arena0/.bb/handoff/claude-handoff-ledger.md.
- [x] Branch audit vs main with ast-grep outline — reuse prior main..92f2fb7 audit plus parent outline delta over all 32 changed Rust files and milestone whole-branch review; no unowned new production API.
- [x] Profile ~100 ms per agreed step — three real RPS journeys measured 83–100 ms median certified-step intervals; SQLite commits median 5.31 ms; repeated module load ~0.05 ms. No speculative optimization; event timings are not CPU attribution. Details/log in handoff ledger.
- [x] Investigate daemon MCP/Host-open timeouts — daemon library tests bypassed kind(test) concurrency group. d1fd32a shares existing four-test budget; deadlines unchanged. Full gate 727/727 passed. Scheduling gap addressed; no claim all possible flakes are eliminated.
- [x] D3b contract written: thr_k94vjvhz6a storage D3b.md; started after D2b
- [x] Audit follow-up: retain separate bounded owners — wire frame admission and program field codecs have distinct error/domain boundaries. No wire→program dependency or speculative common crate.
- [x] Audit follow-up (minor): arena0-test-engine `resolve_test_cache_dir` narrowed to pub(crate), 7505f41.
- [x] Branch audit vs main (ast-grep outline, exported surface) 2026-09-25: new surface matches D-stage designs; ExecEndPhase is the API JSON view of protocol EndPhase (ok).
