export const meta = {
  name: "simplify-execution-model",
  description:
    "Derive arena0's execution and guest-instance model from first principles, attack the derivations and the prior review claims, then return the simplest coherent model",
  phases: [
    { title: "Derive", detail: "two independent first-principles derivations" },
    { title: "Attack", detail: "adversarial attack on the derivations and the prior claims" },
    { title: "Simplify", detail: "final minimal model, deletions, and falsifiers" },
  ],
};

const READ_ONLY =
  "READ-ONLY: do not edit, create, delete, or commit any file in the workspace. Investigation only.\n\n";

const BRIEF = `PROJECT: arena0, a Rust workspace at /data/arena0.

WHAT IT IS: N participants (possibly on different machines) each run the same content-addressed Wasm program. They agree on every public state transition. Each retains a portable receipt that a third party can later verify by replaying the program. Participants also have participant-private state, private inputs (agent callouts), timers, host signatures, and randomness. Untrusted hand-written Wasm is accepted, not only SDK-generated programs. One arena0-node::ExecutionActor exists per execution; the sandbox is arena0-sandbox; the trace, reducer, and receipts are arena0-protocol; portable replay verification is arena0-verify.

DOC UNDER REVIEW (untracked, not on main): /data/arena0/ACTOR_LIFECYCLE.md. It proposes replacing today's fresh-instance-per-call sandbox with one long-lived Wasm instance owned by the ExecutionActor: shared and local state stay resident in guest memory between calls, routine call inputs stop carrying state, the host checkpoints guest memory for crash recovery, read-only calls are run and then rolled back, and a pending shared proposal is run against a memory checkpoint, rolled back while agreement is pending, and re-run when the proposal commits.

PRIOR REVIEW CLAIMS about that doc. Treat these as claims to test, not conclusions.
1. fuel_used is co-signed evidence: TraceEntry::entry_hash (crates/arena0-protocol/src/trace/entry.rs:151) hashes the whole entry including fuel, and StepCommitment signs that hash (crates/arena0-protocol/src/trace/commitment.rs:53). Today fuel is a pure function of (profile, explicit state bytes, event) because every call gets a fresh instance; a resident instance would make it depend on the participant's private history, so honest participants could sign different commitments and fail to agree.
2. Portable full verification cannot repair that: ReceiptBody is {header, outcome, params, trace} with no private records (crates/arena0-protocol/src/execution/certificate.rs:130), and arena0-verify::full replays only public calls and compares fuel exactly (crates/arena0-verify/src/full.rs:296).
3. The instance boundary is what currently guarantees "local calls replace only local state"; a resident instance would let a private call's writes to shared memory persist (Context exposes shared_mut; hand-written Wasm has no type system to stop it).
4. Recovery has no durable genesis: executions.state holds the current aggregate, the activation holds only initial_state: StateHash (crates/arena0-protocol/src/negotiation.rs:616), so a checkpoint is only a replaceable cache if the genesis-plus-full-commit-replay path is named and the commit rows are retained.
5. The doc conflates three things called "checkpoint" (durable recovery checkpoint, in-RAM pre-call rollback image, quiescence boundary) and states no checkpoint write policy or cost model.

Your job is not to repeat or defend these claims. Verify in the code whatever you rely on, and answer the specific task below.`;

phase("Derive");
const derivations = await parallel([
  () =>
    agent(
      READ_ONLY +
        BRIEF +
        `

TASK: derive the minimal execution model for arena0 from the requirements, before reading the design doc.

Do this in order:
(a) For each requirement in the brief (deterministic agreement on public transitions, portable third-party verification of a receipt, participant-private state and inputs that must not reach the wire or a receipt, safe execution of untrusted Wasm, crash and restart recovery, liveness under agent and network delays), state exactly what it FORCES: which fact must be durable, which function must be pure, which input must be common to every participant, which value may be cached, which value may differ per participant.
(b) State the smallest set of invariants whose violation breaks a requirement, and for each one say what breaks and how it becomes observable.
(c) Derive what a call boundary must be: what may stay resident in an untrusted guest between calls, what must be recomputed, what must be part of signed evidence, and what may differ between participants without breaking anything.
(d) Name the requirements that cannot all hold at once, and state the trade explicitly for each.

Then read /data/arena0/ACTOR_LIFECYCLE.md and compare it with your derivation. Name every invariant the doc implies but your derivation does not contain, and every piece of machinery the doc carries that your derivation shows is unnecessary.

Return raw technical prose in markdown. No preamble, no restating of the task. Cite file:line for anything you assert about the code. You may not edit any file.`,
      {
        provider: "codex",
        model: "gpt-6-astra",
        reasoningLevel: "xhigh",
        phase: "Derive",
        label: "Derive from requirements",
      },
    ),
  () =>
    agent(
      READ_ONLY +
        BRIEF +
        `

TASK: extract what the implementation as written actually requires. Read the code, not the documentation prose, for at least these paths:
  crates/arena0-protocol/src/trace/entry.rs, trace/commitment.rs, trace/private.rs
  crates/arena0-protocol/src/execution/reducer/{mod,shared,private,local,terminal}.rs
  crates/arena0-protocol/src/execution/{delta,certificate,state,input}.rs
  crates/arena0-protocol/src/{event,effect}.rs
  crates/arena0-node/src/execution/{mod,actor,guest,inbox,outbox,terminal}.rs
  crates/arena0-sandbox/src/engine/{mod,instance,runtime}.rs, src/engine/imports/*.rs, src/validation.rs
  crates/arena0-verify/src/full.rs
  crates/arena0-sdk-macros/src/program/fresh_abi.rs
  crates/arena0-sdk/src/io_alloc.rs, src/context.rs

Produce:
(1) The invariants the code depends on. Give file:line for each and mark it ENFORCED (by a type, a structural fact, or a check that runs before the consequence) or ASSUMED (true today for a reason that is written nowhere and checked nowhere). Be specific about which ENFORCED invariants would silently degrade to ASSUMED if one live Wasm instance survived across semantic calls.
(2) The durable-versus-derived split. For each durable fact name its single authoritative owner, then list every value that is currently treated as durable while actually being derived, or the reverse, and show which way the dependency runs.
(3) The smallest set of changes that would let a live instance survive across calls without breaking an ENFORCED invariant. If that set is empty, say so and prove it by naming the invariant that cannot survive.

Return raw technical prose in markdown. No preamble. Cite file:line for every claim and write INCONCLUSIVE where you could not verify. You may not edit any file.`,
      {
        provider: "codex",
        model: "gpt-6-astra",
        reasoningLevel: "xhigh",
        phase: "Derive",
        label: "Derive from code",
      },
    ),
]);

const fromRequirements = derivations[0];
const fromCode = derivations[1];
log(
  "derivations: requirements=" +
    (fromRequirements ? "ok" : "missing") +
    " code=" +
    (fromCode ? "ok" : "missing"),
);

phase("Attack");
const attack = await agent(
  READ_ONLY +
    BRIEF +
    `

DERIVATION A (requirements-first):
` +
    (fromRequirements || "(this derivation failed and is omitted; attack what remains)") +
    `

DERIVATION B (code-first):
` +
    (fromCode || "(this derivation failed and is omitted; attack what remains)") +
    `

TASK: attack the material above. Do not summarize it and do not be polite about it.

(1) List every claim in A and B that is false, unproven, or circular, with the code or reasoning that refutes it. "Not clearly argued" is not a refutation; be concrete.
(2) Attack the five prior review claims in the brief the same way, and say explicitly which of them survive.
(3) State the strongest argument AGAINST simplifying, meaning in favor of keeping today's per-call instance model exactly as it is, and the strongest argument FOR the doc's resident-instance model. If the proposed design serves a requirement in the brief that the current model does not, say which one and how.
(4) State where A and B agree. Those points are the load-bearing candidates for the final model, so list them as candidate invariants.
(5) State what cannot be determined from the code and the smallest experiment or check that would settle each unknown.

Return raw technical prose in markdown. No preamble. Cite file:line. You may not edit any file.`,
  {
    provider: "codex",
    model: "gpt-6-astra",
    reasoningLevel: "xhigh",
    phase: "Attack",
    label: "Attack derivations and prior claims",
  },
);

phase("Simplify");
const final = await agent(
  READ_ONLY +
    BRIEF +
    `

DERIVATION A (requirements-first):
` +
    (fromRequirements || "(failed and omitted)") +
    `

DERIVATION B (code-first):
` +
    (fromCode || "(failed and omitted)") +
    `

ATTACK:
` +
    (attack || "(this step failed and is omitted; rely on the derivations alone)") +
    `

TASK: produce the final answer, the simplest coherent execution model for arena0 that satisfies the requirements in the brief. Be decisive and concrete.

(1) Minimal invariant set. For each invariant: what it protects, how it is enforced (type, check, or structure), and the consequence of violation.
(2) The call taxonomy the invariants require: which call kinds must exist and what each may do, derived rather than copied. If the current four-way split (initialize, shared, local, read-only projections) is wrong or over-built, say what the right split is.
(3) The guest-instance lifetime decision. Pick exactly one: (a) fresh instance per semantic call, (b) one resident instance, (c) resident for some call kinds only, (d) something simpler than all of these. Justify it from the invariants, then state precisely what is lost with your choice and how that loss is contained. If you reject the doc's design, name the requirement that makes it impossible rather than the detail that makes it inconvenient.
(4) The deletion list: what in ACTOR_LIFECYCLE.md and in the current implementation should be DELETED rather than modified or extended. Prefer deletion, and name the mechanism that makes each deletion safe.
(5) The cheapest falsifier for your own model: one experiment or check, with the exact command or code path, that would show your model is wrong, and the result that would prove it.
(6) What you are still uncertain about, with the specific evidence that would resolve it.

Return the result through the structured result tool, exactly once. Put the complete final answer in model_markdown (markdown, no preamble). Keep minimal_invariants, deletions, instance_lifetime_decision, and falsifiers consistent with that markdown and in the same order.`,
  {
    provider: "codex",
    model: "gpt-6-astra",
    reasoningLevel: "xhigh",
    phase: "Simplify",
    label: "Simplest coherent model",
    schema: {
      type: "object",
      required: [
        "minimal_invariants",
        "deletions",
        "instance_lifetime_decision",
        "falsifiers",
        "model_markdown",
      ],
      properties: {
        minimal_invariants: { type: "array", items: { type: "string" } },
        deletions: { type: "array", items: { type: "string" } },
        instance_lifetime_decision: { type: "string" },
        falsifiers: { type: "array", items: { type: "string" } },
        model_markdown: { type: "string" },
      },
      additionalProperties: false,
    },
  },
);

return {
  derived_from_code: Boolean(fromCode),
  derived_from_requirements: Boolean(fromRequirements),
  attack_ran: Boolean(attack),
  minimal_invariants: final.minimal_invariants,
  deletions: final.deletions,
  instance_lifetime_decision: final.instance_lifetime_decision,
  falsifiers: final.falsifiers,
  model_markdown: final.model_markdown,
};
