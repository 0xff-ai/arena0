# Cleanup planning and handoff archive

This branch publishes the completed cleanup through code commit `fe3b066`,
along with its planning and decision records. Start with the
[handoff ledger](claude-handoff-ledger.md) for final decisions, commits,
verification results, limitations, and the mapping from original stages to
completed work. The [Stage D task list](tasks-stage-d.md) records completion.

These are historical working documents, not normative API documentation.
Later decisions in the handoff ledger supersede earlier plans and review
claims. In particular, the user explicitly authorized this remote branch on
2026-09-25, superseding the earlier no-push instructions preserved in these
records. Local filesystem paths, thread IDs, and references to retained logs
describe the original development environment; those logs and transcripts are
not part of this archive. Links between bundled documents are relative.

| Documents | Purpose |
|---|---|
| [Original design](design.md) | Stages A, B, T, C, E, and D |
| [D2b design](design-d2b.md), [contract](contract-d2b.md) | Bounded codecs and derived encodings |
| [D2c contract](contract-d2c.md) | Durable fact ownership |
| [D2d design](design-d2d.md) | Plain-argument sandbox calls |
| [D2e design](design-d2e.md) | Context types parameterized by handler mode |
| [D3b design](design-d3b.md), [contract](contract-d3b.md) | Real-Host scenarios and native harness removal |
| [Structural audit](audit-shape.md), [Stage D survey](survey-d.md) | Consolidation findings |
| [D2a decisions](decision-d2a.md), [storage notes](storage-notes.md) | Owner decisions and supporting analysis |
| [Milestone review brief](stage-d-milestone.md), [report](stage-d-milestone-review.md) | Independent review; parent rulings are in the ledger |
| [Projection-test contract](terminal-projection-tests.md) | Restored terminal rendering and outcome coverage |

Validation of the code is recorded in the ledger: full check/test gate,
727 host tests, doctests, and 25 program tests; subsequent import-only cleanup
passed targeted checks and preserved all ten generated Wasm hashes. Adding
this archive does not change the tested code.
