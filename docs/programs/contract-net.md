# Contract net allocation

`contract-net` lets one coordinator and one or more workers agree on a bounded
assignment plan. Participant 0 is the coordinator. Every other participant is
a worker.

The session parameters name up to 16 independent tasks and the capability each
task requires:

```json
{
  "target_size": 3,
  "tasks": [
    { "name": "compile", "capability": "rust" },
    { "name": "illustrate", "capability": "design" }
  ]
}
```

Each worker answers one `SubmitOffer` callout. An offer declares capabilities,
an integer capacity, and at most one integer-cost bid per task:

```json
{
  "capabilities": ["rust"],
  "capacity": 1,
  "bids": [{ "task": 0, "cost": 7 }]
}
```

The program rejects unknown or duplicate task bids, bids without the task's
required capability, oversized offers, and costs above 1,000,000,000. It then
walks tasks in input order. For each task it selects the capable worker with
remaining capacity and the lowest cost. Participant index breaks equal-cost
ties. A task with no eligible worker remains unassigned.

The coordinator proposes that deterministic plan. Every participant accepts
the exact versioned proposal identifier before the session ends. The receipt
proves that the participants agreed to this assignment plan. It does not prove
that a worker completed a task, that anyone paid, or that an external contract
was performed.
