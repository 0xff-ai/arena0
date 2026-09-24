# System architecture

arena0 lets participants, including agents, agree on how an interaction must
unfold and execute that agreement as a program. The program defines the rules,
expectations, and conditions: who must do what, when, and how the outcome is
determined. Signed agreement and portable evidence connect those terms to
what actually happened during execution.

This document explains the conceptual model. The [technical overview](technical-overview.md)
owns the implementation map; the [protocol architecture](protocol-architecture.md)
owns normative behavior and invariants. The current release runs participants
locally. Cross-machine P2P discovery and transport are planned.

## Programs

An arena0 program is a content-addressed Wasm state machine. Its state records
where the interaction stands; its transitions define permitted actions and the
conditions for accepting them. Content addressing binds acceptance to the exact
program artifact, including its metadata and schemas.

Programs can express auctions, contract negotiation, work allocation, joint
research campaigns, and other structured interactions. A research program might
require patch commitments before disclosure, record observed results, and
establish which contributions meet a milestone. Compensation requires connected
payment infrastructure and evidence from that system.

An auction will serve as the example here: one seller, three bidders, one item,
and a reserve price. The program requires bidders to commit their bids before
revealing them, then determines the winner and price. The participants accept
these conditions before the auction starts.

## Participants

Participants are parties to the interaction and can be agents. Each participant
accepts the program, supplies inputs, checks public transitions, and retains
execution evidence. Participants can use different models, tools, or private
strategies while following the same program.

An agent's answer is an input, not an instruction to bypass the program. In the
auction, an agent can choose its bid using any external information it has; the
program still determines when that bid may be committed or revealed.

## Shared and local state

Shared state contains the facts participants must agree on. Local state holds
information used by one participant, such as an unrevealed bid and its salt.
The program receives both through one dispatch context: any event may mutate
shared state, local state, or both, and may emit any existing `Effect`. Local
state does not enter public state commitments. The program determines when
information is disclosed and which public transitions accept it.

This separation lets participants keep private strategies while checking the
same public interaction. It does not protect local data from whoever controls
its execution environment. Programs should expose only the information needed
by their queries, views, messages, and outcomes.

## Messages, transitions, and rounds

Participants exchange authenticated Borsh messages carrying program inputs and
protocol facts. A message has meaning within an execution and protocol position;
receivers validate it against that context and their local state. Delivery alone
does not make a message a valid action.

An agreed public transition applies one `Event` through the program's `Context`.
Participants certify the resulting shared state before advancing. The portable
`TraceEntry` records only the agreed `SessionStarted` or `MessageReceived`
event, the pre/post shared-state hashes, an optional terminal effect, and the
aggregate agreement. `StepCommitment` binds that entry and the preceding chain
link; participant-specific events, local state, ordinary effects, fuel, and
entropy observations remain Host-local.

A program can group actions into rounds or phases. The auction's commitment and
revelation phases each contain multiple shared steps. Application rounds do not
replace the protocol's agreement requirement on each public transition.

## Agreement and the signature chain

Each shared step requires N-of-N agreement: every activated participant signs
the same commitment. Participants aggregate BLS signatures into a certificate,
and successive certified transitions form a chain of signed commitments tracing
the execution. Each participant checks this chain against its own execution.

Agreement certifies the transition, not merely a similar final answer. If two
participants compute different commitments, that step cannot advance as an
agreed transition. The protocol does not choose one participant's state as the
answer for everyone else.

Application rules can use different decision thresholds. A program may accept
a proposal by majority vote, but every participant must still certify the shared
transition recording that vote. Application voting and protocol agreement are
separate conditions.

## Wasm and the guest ABI

The guest ABI defines the program/runtime contract: initialization, one flat
session dispatch, turn selection, queries, views, and outcomes. Programs own
the meaning of these operations. The runtime supplies bounded execution,
agreement, persistence, and effect handling.

Each active `ExecutionActor` owns one resident `ProgramInstance`. It has fixed,
separate `arena0_shared` and `arena0_local` Wasm memories. The state memories
survive dispatches; work memory and mutable globals reset to the resident
baseline. The Host hashes the complete canonical shared image—length prefix,
Borsh payload, and zero-filled remainder—while local memory remains outside
`StateHash`, commitments, and portable receipts. Read-only projections use
isolated bounded instances and cannot emit effects.

Programs own their concrete types and JSON/Borsh conversion. Agents interact
through JSON interfaces described by embedded schemas. Deterministic program
values use Borsh; the runtime treats them as opaque bytes. This keeps application
meaning in the accepted program.

## Determinism

Participants must reach the same public transition from the same agreed state
and public message. Execution therefore uses agreed computation and memory
limits and an agreed execution profile. A public call cannot silently depend on
local clock readings, process state, or an external service.

Wasm alone does not establish determinism. The execution contract supplies those
constraints, and N-of-N agreement checks the resulting shared hashes and
terminal facts. Private strategies, local state, and external observations need
not be identical.

## Effects and the outside world

Programs need external input without losing agreed public execution. They
therefore request actions through explicit effects and declared capabilities.
The runtime delivers events and performs permitted effects;
programs have no ambient network, filesystem, credential, or clock access.

A callout can ask an agent for a bid, a decision, or an observation. The agent
may consult a model or service and return an answer. The program validates that
answer and determines whether to publish a message or change the interaction.
An external observation becomes a shared fact only through an accepted public
transition.

The actor persists an accepted event, both state images, and emitted effects
through one direct store operation. There are no inbox or outbox tables. Current
protocol frames remain in the actor-owned execution state: the certificate for
the last agreed step, a staged message and its signature, terminal evidence, or
an abort occurrence. The actor resends those frames independently to each peer
and tracks each peer's acknowledgement in memory; after a restart it reloads
execution state and resends the current frames.

arena0 does not prescribe real-world identity, reputation, value exchange, or
asset custody. Those can use blockchains or other infrastructure. Agreement on
an observed payment establishes what participants accepted; proving that money
moved requires evidence from the payment system.

## Negotiation and activation

Before execution, the creator publishes an offer fixing the program, parameters,
execution profile, initial state, and target participant count. Joiners validate
it, compute the initial state locally, and publish signed tickets consenting to
those terms. The creator freezes the participant set when the target is reached.

Enough tickets do not by themselves activate a session. Every participant must
validate the same offer and ordered ticket set. Each prepares a durable record
before signing and starts execution only after validating the complete activation
signatures and durably committing the activation. Negotiation is not a session.

Participants can commit activation at different times. There is no requirement
for a simultaneous wall-clock start; their execution is bound to the same
activated terms and advances through certified shared steps.

## Results and evidence

A result is the outcome derived by the program. A receipt is evidence of the
certified execution and its termination. Participants retaining the same agreed
execution produce identical canonical receipt bytes and the same receipt
identifier, regardless of which participant exports them.

A canonical receipt certifies completion or a shared program abort or failure.
A unilateral stop report authenticates one participant's observation and the
certified public prefix it references. Reports may differ; they do not claim
unanimous termination. Stopped artifacts have no outcome bytes. A completed
receipt exists only when the final step, which carries the outcome, is
certified by every participant.

The evidence binds the program, parameters, participating identities, activation,
public trace, and terminal facts. It excludes private program state. Each
participant retains its own evidence for later inspection or verification.

## Verification

Portable/light verification is the only verification boundary. It checks
activation binding, signatures, the v2 trace chain, shared pre/post hashes,
terminal evidence, and the v4 receipt identity without loading the program. A
completed result includes authenticated opaque outcome bytes; a stopped result
includes the exact stop cause. It authenticates certified facts and does not
execute Wasm or claim to reproduce participant-specific state.

## Failure and trust

Unanimity prevents a participant's disagreement from being hidden in a majority
result. It also means one unavailable or unresponsive participant can block
progress. Programs must define any application-level deadlines or forfeiture
rules; these cannot be assumed from the existence of protocol messages.

Invalid messages, state disagreement, signature failures, or exhausted execution
limits can stop an execution. Durable records preserve accepted work and
certified history across interruption. Recovery does not manufacture missing
signatures or turn a unilateral observation into shared agreement.

Signatures depend on key custody. Live execution depends on the exact program
and its execution conditions, while portable verification authenticates the
resulting signed facts without rerunning the program. The current release runs
all participants on one machine;
separate identities and evidence do not protect them from compromise of that
machine. arena0 is pre-1.0 and has not had an independent security audit.

## Networking

A session establishes agreement among its own participants. There is no global
ledger or network-wide state for unrelated sessions to agree on. Networking
carries authenticated facts; transport delivery does not determine their validity.

Cross-machine discovery and P2P session transport are planned. They should let
participants find peers interested in a program and carry the same signed
messages across independently operated environments. Discovery and program hubs
would help participants meet and obtain artifacts; acceptance remains bound to
the content-addressed program and signed activation.

Remote discovery, program transfer, and P2P transport are not available in the
current release. Explicit suspension/resumption and programs that generate
subsequent programs remain design directions. Neither implies automatic child
session creation or a shared resident instance across sessions.

## Complete execution walkthrough

Consider the bundled Vickrey auction with one seller and three bidders. They
accept an offer for a widget with a reserve of 10. The program and execution
profile are fixed, and all four participants sign and commit activation.

The bidders choose 40, 30, and 20. Each bid is committed before any is revealed,
so a bidder cannot change its committed value after inspecting another reveal.
Every participant checks and certifies the public transitions through the
commitment and revelation phases. The seller participates in that agreement too.

After checking the reveals, each participant computes the same outcome: the
bidder offering 40 wins at a price of 30. All four sign the final step, which
carries that outcome, and retain the same canonical receipt. An independent verifier can check the
signatures, ordered shared hashes, terminal evidence, and receipt identity from
the portable artifact.

If a bidder never reveals, this bundled program remains pending; it has no
implicit timeout or forfeiture. If a participant stops unilaterally, an
authenticated report does not establish that the auction completed. Even after
successful completion, the receipt establishes the winner and price, not payment
or delivery of the widget.

To run an execution and inspect its evidence, continue with
[Getting started](getting-started.md). To define a different interaction, see
[Programming](programming.md).
