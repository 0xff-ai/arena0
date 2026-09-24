//! Bounded contract-net allocation.
//!
//! Participant 0 is the coordinator. Every other participant publishes one
//! capacity-limited offer. The coordinator derives a proposal by considering
//! tasks in input order and choosing the lowest integer cost, breaking ties by
//! participant index. Every participant then accepts that exact proposal.
//!
//! The receipt proves agreement on the assignment plan only. It does not claim
//! that work finished, money moved, or any external contract was performed.

use std::fmt::Write;

use arena0::prelude::*;
use arena0_primitives::agreement::{Agreement, Status as AgreementStatus};
use arena0_primitives::ballot::{ProposalId, Tally, Vote};

const COORDINATOR: Participant = Participant::new(0);
const PROPOSAL_VERSION: u32 = 1;
const MAX_PARTICIPANTS: u32 = 16;
const MAX_TASKS: usize = 16;
const MAX_TEXT_BYTES: usize = 48;
const MAX_COST: u64 = 1_000_000_000;

/// Session configuration. Task position is its stable task identifier.
#[arena0::data]
pub struct Params {
    pub target_size: u32,
    pub tasks: Vec<Task>,
}

/// One independent task and its required worker capability.
#[arena0::data]
pub struct Task {
    pub name: String,
    pub capability: String,
}

/// Integer cost offered for one task identifier.
#[arena0::data]
pub struct Bid {
    pub task: u16,
    pub cost: u64,
}

/// One worker's bounded capability, capacity, and cost declaration.
#[arena0::data]
pub struct WorkerOffer {
    pub capabilities: Vec<String>,
    pub capacity: u16,
    pub bids: Vec<Bid>,
}

/// Result for one input task.
#[arena0::data]
pub struct Assignment {
    pub task: u16,
    pub award: Award,
}

/// A task is assigned to one capable worker or remains unassigned.
#[arena0::data]
pub enum Award {
    Assigned { worker: Participant, cost: u64 },
    Unassigned,
}

/// Exact task-ordered allocation considered by the agreement primitive.
#[arena0::data]
pub struct AssignmentPlan {
    pub assignments: Vec<Assignment>,
}

#[arena0::message]
pub enum Message {
    Offer(WorkerOffer),
    Proposal { plan: AssignmentPlan },
    Accept { proposal: ProposalId },
}

#[arena0::callouts]
pub enum Callout {
    /// Submit the local worker's capability-limited integer-cost offer.
    #[arena0::callout(output = WorkerOffer)]
    SubmitOffer {
        tasks: Vec<Task>,
        maximum_capacity: u16,
        maximum_cost: u64,
    },
}

// The host owns terminal behavior. `Transition::End` derives the outcome and
// emits `SessionEnd`; the program does not model a second terminal phase.
#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "Collecting worker offers")]
    CollectingOffers,
    #[phase(description = "Accepting the exact assignment proposal")]
    ReviewingProposal,
}

/// Receipt projection proving only acceptance of an assignment plan.
#[arena0::outcome]
pub struct Outcome {
    pub proposal: ProposalId,
    pub plan: AssignmentPlan,
}

#[arena0::state(max = 65536)]
pub struct Shared {
    #[phase]
    phase: Phase,
    target_size: u32,
    tasks: Vec<Task>,
    /// Participant-indexed. The coordinator's slot remains `None`.
    offers: Vec<Option<WorkerOffer>>,
    #[primitive]
    agreement: Agreement<AssignmentPlan>,
}

/// Participant-private guards for one-shot effects.
#[arena0::local]
#[derive(Default)]
pub struct Local {
    offer_sent: bool,
    proposal_sent: bool,
    accepted_proposal: Option<ProposalId>,
}

impl Shared {
    fn expected_offer_writer(&self) -> Option<Participant> {
        (1..self.offers.len())
            .find(|&index| self.offers[index].is_none())
            .and_then(|index| Participant::try_from(index).ok())
    }

    fn all_offers_received(&self) -> bool {
        self.offers.len() > 1 && self.offers[1..].iter().all(Option::is_some)
    }

    fn expected_writer(&self) -> Option<Participant> {
        match self.phase() {
            Phase::CollectingOffers => self
                .expected_offer_writer()
                .or_else(|| self.all_offers_received().then_some(COORDINATOR)),
            Phase::ReviewingProposal => self
                .agreement
                .ballot()
                .and_then(|ballot| ballot.next_voter()),
        }
    }

    fn plan(&self) -> AssignmentPlan {
        allocate(&self.tasks, &self.offers)
    }
}

#[arena0::program(
    name = "contract-net",
    display_name = "Contract Net Allocation",
    version = "1.0.0",
    description = "Bounded capability and integer-cost task allocation with exact proposal acceptance",
    participants = 2..=16,
    capabilities(auto)
)]
pub mod contract_net {
    use super::*;
    use arena0::ProgramTransition;

    type Shared = super::Shared;
    type Local = super::Local;
    type Message = super::Message;
    type Callout = super::Callout;
    type Input = super::Input;
    type Params = super::Params;
    type Outcome = super::Outcome;

    fn outcome(state: &Shared) -> Outcome {
        let accepted = state
            .agreement
            .accepted()
            .expect("session ends only after the assignment proposal is accepted");
        Outcome {
            proposal: accepted.id,
            plan: accepted.value.clone(),
        }
    }

    fn writer(state: &Shared) -> Option<Participant> {
        state.expected_writer()
    }

    fn initialize(shared: &mut Shared, params: Params) -> Result<(), ProgramFault> {
        params.validate().map_err(|error| anyhow!(error))?;
        shared.target_size = params.target_size;
        shared.tasks = params.tasks;
        Ok(())
    }

    fn on_session_started(
        ctx: &mut Context<Shared, Local>,
        ensemble: &Ensemble,
    ) -> Result<ProgramTransition<ContractNet>, ProgramFault> {
        if ensemble.len() != ctx.shared().target_size as usize {
            return Err(anyhow!(
                "expected {} participants, got {}",
                ctx.shared().target_size,
                ensemble.len()
            )
            .into());
        }
        ctx.shared_mut().offers = vec![None; ensemble.len()];
        Ok(Transition::Stay)
    }

    fn on_react(
        ctx: &mut Context<Shared, Local>,
    ) -> Result<ProgramTransition<ContractNet>, ProgramFault> {
        if ctx.shared().expected_writer() != Some(ctx.me()) {
            return Ok(Transition::Stay);
        }

        match ctx.shared().phase() {
            Phase::CollectingOffers if ctx.me() == COORDINATOR => {
                if !ctx.local().proposal_sent {
                    let plan = ctx.shared().plan();
                    let participant_count = ctx.ensemble().len();
                    apply_proposal(ctx.shared_mut(), plan.clone(), participant_count)?;
                    ctx.effects().broadcast(&Message::Proposal { plan });
                    ctx.mutate_local(|local| local.proposal_sent = true);
                    return Ok(Transition::To(Phase::ReviewingProposal));
                }
            }
            Phase::CollectingOffers => {}
            Phase::ReviewingProposal => {
                let expected_plan = ctx.shared().plan();
                let Some(proposal) = ctx.shared().agreement.proposal() else {
                    return Err(anyhow!("reviewing phase has no proposal").into());
                };
                if proposal.value != &expected_plan {
                    return Err(
                        anyhow!("stored proposal differs from deterministic allocation").into(),
                    );
                }
                let proposal_id = proposal.id;
                if ctx.local().accepted_proposal != Some(proposal_id) {
                    let me = ctx.me();
                    let tally = apply_accept(ctx.shared_mut(), me, proposal_id)?;
                    ctx.effects().broadcast(&Message::Accept {
                        proposal: proposal_id,
                    });
                    ctx.mutate_local(|local| local.accepted_proposal = Some(proposal_id));
                    return transition_for_tally(tally);
                }
            }
        }
        Ok(Transition::Stay)
    }

    fn callout(ctx: &Context<Shared, Local>) -> Option<Callout> {
        (ctx.shared().phase() == Phase::CollectingOffers
            && ctx.me() != COORDINATOR
            && ctx.shared().expected_writer() == Some(ctx.me())
            && !ctx.local().offer_sent)
            .then(|| {
                let tasks = ctx.shared().tasks.clone();
                let maximum_capacity = tasks.len() as u16;
                callouts::SubmitOffer {
                    tasks,
                    maximum_capacity,
                    maximum_cost: MAX_COST,
                }
                .into()
            })
    }

    fn on_input(
        ctx: &mut Context<Shared, Local>,
        input: Input,
    ) -> arena0::anyhow::Result<ProgramTransition<ContractNet>> {
        let Input::SubmitOffer(offer) = input;
        if ctx.shared().phase() != Phase::CollectingOffers
            || ctx.shared().expected_offer_writer() != Some(ctx.me())
        {
            return Err(anyhow!("offer is not due from this participant"));
        }
        offer
            .validate(&ctx.shared().tasks)
            .map_err(|error| anyhow!(error))?;
        let from = ctx.me();
        apply_offer(ctx.shared_mut(), from, offer.clone());
        ctx.effects().broadcast(&Message::Offer(offer));
        ctx.mutate_local(|local| local.offer_sent = true);
        Ok(Transition::Stay)
    }

    fn on_message(
        ctx: &mut Context<Shared, Local>,
        from: Participant,
        message: Message,
    ) -> MessageApply<ContractNet> {
        if ctx.shared().expected_writer() != Some(from) {
            return Ok(ApplyDecision::Reject);
        }

        match message {
            Message::Offer(offer) => {
                if ctx.shared().phase() != Phase::CollectingOffers
                    || from == COORDINATOR
                    || offer.validate(&ctx.shared().tasks).is_err()
                {
                    return Ok(ApplyDecision::Reject);
                }
                apply_offer(ctx.shared_mut(), from, offer);
                Ok(ApplyDecision::Accept(Transition::Stay))
            }
            Message::Proposal { plan } => {
                if ctx.shared().phase() != Phase::CollectingOffers
                    || from != COORDINATOR
                    || !ctx.shared().all_offers_received()
                    || plan != ctx.shared().plan()
                {
                    return Ok(ApplyDecision::Reject);
                }

                let participant_count = ctx.ensemble().len();
                if apply_proposal(ctx.shared_mut(), plan, participant_count).is_err() {
                    return Ok(ApplyDecision::Reject);
                }
                Ok(ApplyDecision::Accept(Transition::To(
                    Phase::ReviewingProposal,
                )))
            }
            Message::Accept { proposal } => {
                if ctx.shared().phase() != Phase::ReviewingProposal {
                    return Ok(ApplyDecision::Reject);
                }
                let Ok(tally) = apply_accept(ctx.shared_mut(), from, proposal) else {
                    return Ok(ApplyDecision::Reject);
                };
                Ok(ApplyDecision::Accept(transition_for_tally(tally)?))
            }
        }
    }

    fn view(state: &Shared, ensemble: &Ensemble, vp: &Viewport) -> View {
        View::new()
            .header(vp.fit_text(format!(
                "Contract net - {} task{}",
                state.tasks.len(),
                if state.tasks.len() == 1 { "" } else { "s" }
            )))
            .agents(vp.fit_text(render_agents(state, ensemble)))
            .state(vp.fit_text(render_state(state)))
            .status_bar(vp.fit_text(render_status(state)))
    }

    fn on_query(_shared: &Shared, _: ()) {}

    fn render_agents(state: &Shared, ensemble: &Ensemble) -> String {
        let mut output = String::new();
        for index in 0..ensemble.len() {
            let participant = Participant::try_from(index)
                .expect("validated participant count fits a participant index");
            if participant == COORDINATOR {
                let _ = writeln!(output, "P{index}: coordinator");
                continue;
            }
            let offer = state.offers.get(index).and_then(Option::as_ref);
            match offer {
                Some(offer) => {
                    let _ = writeln!(output, "P{index}: worker, capacity {}", offer.capacity);
                }
                None => {
                    let _ = writeln!(output, "P{index}: worker, offer pending");
                }
            }
        }
        output
    }

    fn render_state(state: &Shared) -> String {
        let mut output =
            String::from("Tasks\nid  name                 capability           assignment\n");
        let plan = state.agreement.proposal().map(|proposal| proposal.value);
        for (index, task) in state.tasks.iter().enumerate() {
            let award = plan.and_then(|plan| plan.assignments.get(index)).map_or(
                "pending".to_string(),
                |assignment| match assignment.award {
                    Award::Assigned { worker, cost } => {
                        format!("P{} @ {cost}", worker.index())
                    }
                    Award::Unassigned => "unassigned".to_string(),
                },
            );
            let _ = writeln!(
                output,
                "{index:<3} {:<20} {:<20} {award}",
                task.name, task.capability
            );
        }
        output
    }

    fn render_status(state: &Shared) -> String {
        let offers = state.offers.iter().skip(1).flatten().count();
        let workers = state.offers.len().saturating_sub(1);
        match state.agreement.status() {
            AgreementStatus::Idle => format!("collecting offers - {offers}/{workers}"),
            AgreementStatus::Voting => {
                let remaining = state
                    .agreement
                    .ballot()
                    .and_then(|ballot| match ballot.tally() {
                        Tally::Pending { remaining, .. } => Some(remaining),
                        Tally::NotStarted | Tally::Accepted { .. } | Tally::Rejected { .. } => None,
                    })
                    .unwrap_or(0);
                format!("reviewing proposal - {remaining} acceptance(s) remaining")
            }
            AgreementStatus::Accepted => "assignment plan accepted".to_string(),
            AgreementStatus::Rejected => "assignment plan rejected".to_string(),
        }
    }

    fn apply_offer(state: &mut Shared, from: Participant, offer: WorkerOffer) {
        state.offers[from.index()] = Some(offer);
    }

    fn apply_proposal(
        state: &mut Shared,
        plan: AssignmentPlan,
        participant_count: usize,
    ) -> Result<(), arena0::anyhow::Error> {
        let eligible = (0..participant_count)
            .map(|index| {
                Participant::try_from(index)
                    .expect("validated participant count fits a participant index")
            })
            .collect();
        let threshold = participant_count as u16;
        state
            .agreement
            .propose(PROPOSAL_VERSION, plan, eligible, threshold)
            .map(|_| ())
            .map_err(|error| anyhow!(error))
    }

    fn apply_accept(
        state: &mut Shared,
        from: Participant,
        proposal: ProposalId,
    ) -> Result<Tally, arena0::anyhow::Error> {
        state
            .agreement
            .vote(from, proposal, Vote::Accept)
            .map_err(|error| anyhow!(error))
    }

    fn transition_for_tally(tally: Tally) -> Result<ProgramTransition<ContractNet>, ProgramFault> {
        match tally {
            Tally::Pending { .. } => Ok(Transition::Stay),
            Tally::Accepted { .. } => Ok(Transition::End),
            Tally::NotStarted | Tally::Rejected { .. } => {
                Err(anyhow!("accept-only ballot reached an impossible result").into())
            }
        }
    }
}

impl Params {
    fn validate(&self) -> Result<(), String> {
        if !(2..=MAX_PARTICIPANTS).contains(&self.target_size) {
            return Err(format!(
                "target_size must be between 2 and {MAX_PARTICIPANTS}"
            ));
        }
        if self.tasks.is_empty() || self.tasks.len() > MAX_TASKS {
            return Err(format!("task count must be between 1 and {MAX_TASKS}"));
        }
        for (index, task) in self.tasks.iter().enumerate() {
            validate_text("task name", &task.name)?;
            validate_text("task capability", &task.capability)?;
            if self.tasks[..index]
                .iter()
                .any(|prior| prior.name == task.name)
            {
                return Err(format!("duplicate task name {:?}", task.name));
            }
        }
        Ok(())
    }
}

impl WorkerOffer {
    fn validate(&self, tasks: &[Task]) -> Result<(), String> {
        if usize::from(self.capacity) > tasks.len() {
            return Err("worker capacity exceeds the task count".to_string());
        }
        if self.capabilities.len() > MAX_TASKS {
            return Err("worker declares too many capabilities".to_string());
        }
        for (index, capability) in self.capabilities.iter().enumerate() {
            validate_text("worker capability", capability)?;
            if self.capabilities[..index].contains(capability) {
                return Err(format!("duplicate worker capability {capability:?}"));
            }
        }
        if self.bids.len() > tasks.len() {
            return Err("worker declares too many bids".to_string());
        }
        for (index, bid) in self.bids.iter().enumerate() {
            let Some(task) = tasks.get(usize::from(bid.task)) else {
                return Err(format!("bid references unknown task {}", bid.task));
            };
            if bid.cost > MAX_COST {
                return Err(format!("bid cost exceeds {MAX_COST}"));
            }
            if !self.capabilities.contains(&task.capability) {
                return Err(format!(
                    "bid for task {} lacks capability {:?}",
                    bid.task, task.capability
                ));
            }
            if self.bids[..index]
                .iter()
                .any(|prior| prior.task == bid.task)
            {
                return Err(format!("duplicate bid for task {}", bid.task));
            }
        }
        Ok(())
    }
}

fn validate_text(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if value.len() > MAX_TEXT_BYTES {
        return Err(format!("{label} must not exceed {MAX_TEXT_BYTES} bytes"));
    }
    Ok(())
}

fn allocate(tasks: &[Task], offers: &[Option<WorkerOffer>]) -> AssignmentPlan {
    let mut remaining: Vec<u16> = offers
        .iter()
        .map(|offer| offer.as_ref().map_or(0, |offer| offer.capacity))
        .collect();
    let mut assignments = Vec::with_capacity(tasks.len());

    for (task_index, task) in tasks.iter().enumerate() {
        let best = offers
            .iter()
            .enumerate()
            .skip(1)
            .filter_map(|(worker_index, offer)| {
                let offer = offer.as_ref()?;
                if remaining[worker_index] == 0 || !offer.capabilities.contains(&task.capability) {
                    return None;
                }
                let bid = offer
                    .bids
                    .iter()
                    .find(|bid| usize::from(bid.task) == task_index)?;
                let worker = Participant::try_from(worker_index).ok()?;
                Some((bid.cost, worker.as_u8(), worker_index, worker))
            })
            .min_by_key(|(cost, participant, _, _)| (*cost, *participant));

        let award = if let Some((cost, _, worker_index, worker)) = best {
            remaining[worker_index] -= 1;
            Award::Assigned { worker, cost }
        } else {
            Award::Unassigned
        };
        assignments.push(Assignment {
            task: task_index as u16,
            award,
        });
    }

    AssignmentPlan { assignments }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0::testing::{FaultStatus, Harness, TestHarness};
    use arena0::types::{ColorDepth, Slot};

    fn tasks() -> Vec<Task> {
        vec![
            Task {
                name: "compile".to_string(),
                capability: "rust".to_string(),
            },
            Task {
                name: "review".to_string(),
                capability: "rust".to_string(),
            },
            Task {
                name: "illustrate".to_string(),
                capability: "design".to_string(),
            },
        ]
    }

    fn offer(capabilities: &[&str], capacity: u16, bids: &[(u16, u64)]) -> WorkerOffer {
        WorkerOffer {
            capabilities: capabilities
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            capacity,
            bids: bids
                .iter()
                .map(|(task, cost)| Bid {
                    task: *task,
                    cost: *cost,
                })
                .collect(),
        }
    }

    #[test]
    fn greedy_allocation_checks_capability_capacity_cost_and_ties() {
        let offers = vec![
            None,
            Some(offer(&["rust"], 1, &[(0, 5), (1, 1)])),
            Some(offer(&["rust", "design"], 2, &[(0, 5), (1, 2), (2, 8)])),
        ];
        let plan = allocate(&tasks(), &offers);
        assert_eq!(
            plan,
            AssignmentPlan {
                assignments: vec![
                    Assignment {
                        task: 0,
                        award: Award::Assigned {
                            worker: Participant::new(1),
                            cost: 5,
                        },
                    },
                    Assignment {
                        task: 1,
                        award: Award::Assigned {
                            worker: Participant::new(2),
                            cost: 2,
                        },
                    },
                    Assignment {
                        task: 2,
                        award: Award::Assigned {
                            worker: Participant::new(2),
                            cost: 8,
                        },
                    },
                ],
            }
        );
    }

    #[test]
    fn invalid_offers_are_rejected_at_input_and_message_boundaries() {
        let params = Params {
            target_size: 2,
            tasks: tasks(),
        };
        let coordinator = PeerId([0; 32]);
        let worker = PeerId([1; 32]);
        let invalid = [
            offer(&["design"], 1, &[(0, 4)]),
            offer(&["rust"], 1, &[(0, 4), (0, 5)]),
            offer(&["rust"], 4, &[(0, 4)]),
        ];

        // The input boundary classifies invalid local answers as rejected and
        // does not mark the worker's offer as sent.
        let mut input_harness = TestHarness::<ContractNet>::with_peer_id(worker, params.clone());
        input_harness.session_started(coordinator);
        for offer in invalid.iter().cloned() {
            let before = input_harness.shared_hash();
            let result = input_harness.input(Input::SubmitOffer(offer));
            assert!(matches!(result.fault, FaultStatus::Rejected(_)));
            assert!(!result.has_broadcast());
            assert_eq!(input_harness.shared_hash(), before);
            assert!(!input_harness.local().offer_sent);
        }

        // The shared message boundary rejects the same malformed offers before
        // state mutation, trace creation, or effects.
        let mut message_harness = TestHarness::<ContractNet>::new(params);
        message_harness.session_started(worker);
        for offer in invalid {
            let before = message_harness.shared_hash();
            let result = message_harness.message(worker, Message::Offer(offer));
            assert!(result.rejected);
            assert!(result.effects.is_empty());
            assert!(result.records.is_empty());
            assert_eq!(message_harness.shared_hash(), before);
        }
    }

    #[test]
    fn native_scenario_accepts_only_the_exact_plan() {
        let params = Params {
            target_size: 2,
            tasks: vec![Task {
                name: "compile".to_string(),
                capability: "rust".to_string(),
            }],
        };
        let worker = PeerId([1; 32]);
        let coordinator = PeerId([0; 32]);
        let mut coordinator_harness =
            TestHarness::<ContractNet>::with_peer_id(coordinator, params.clone());
        let mut worker_harness = TestHarness::<ContractNet>::with_peer_id(worker, params);
        let started = coordinator_harness.session_started(worker);
        assert!(matches!(started.fault, FaultStatus::None));

        let worker_started = worker_harness.session_started(coordinator);
        assert!(matches!(worker_started.fault, FaultStatus::None));
        let worker_offer = offer(&["rust"], 1, &[(0, 7)]);
        let offered = worker_harness.resolve_callout::<callouts::SubmitOffer>(worker_offer);
        assert!(matches!(offered.fault, FaultStatus::None));
        let offer_message = offered
            .messages::<Message>()
            .into_iter()
            .find(|message| matches!(message, Message::Offer(_)))
            .expect("worker emits the offer");
        let offered = coordinator_harness.message(worker, offer_message);
        assert!(matches!(offered.fault, FaultStatus::None));
        assert!(
            offered
                .messages::<Message>()
                .iter()
                .any(|message| matches!(message, Message::Proposal { .. })),
            "coordinator emits the deterministic proposal"
        );
        let proposal = offered
            .messages::<Message>()
            .into_iter()
            .find(|message| matches!(message, Message::Proposal { .. }))
            .expect("coordinator emits the deterministic proposal");
        let id = coordinator_harness
            .shared()
            .agreement
            .proposal()
            .expect("proposal is active")
            .id;

        // The worker rejects a proposal whose plan is not the deterministic
        // allocation, before the real proposal is delivered.
        let wrong_plan = AssignmentPlan {
            assignments: vec![Assignment {
                task: 0,
                award: Award::Unassigned,
            }],
        };
        let wrong_proposal =
            worker_harness.message(coordinator, Message::Proposal { plan: wrong_plan });
        assert!(wrong_proposal.rejected);

        // Deliver the producer's proposal to the worker. The coordinator's
        // ballot puts its own vote first, so inject that distinct vote event on
        // the coordinator replica before delivering it to the worker. This is
        // not a re-delivery of a producer broadcast.
        let proposed = worker_harness.message(coordinator, proposal);
        assert!(matches!(proposed.fault, FaultStatus::None));
        let wrong = ProposalId {
            version: PROPOSAL_VERSION,
            hash: [0xff; 32],
        };
        let wrong_accept = worker_harness.message(coordinator, Message::Accept { proposal: wrong });
        assert!(wrong_accept.rejected);

        let coordinator_vote =
            coordinator_harness.message(coordinator, Message::Accept { proposal: id });
        assert!(matches!(coordinator_vote.fault, FaultStatus::None));
        let worker_vote = worker_harness.message(coordinator, Message::Accept { proposal: id });
        let worker_accept = worker_vote
            .messages::<Message>()
            .into_iter()
            .find(|message| matches!(message, Message::Accept { .. }))
            .expect("worker accepts the exact proposal");
        let finished = coordinator_harness.message(worker, worker_accept);
        assert!(matches!(finished.fault, FaultStatus::None));
        assert!(finished.has_session_end());
        assert_eq!(
            coordinator_harness.shared().agreement.status(),
            AgreementStatus::Accepted
        );
        assert_eq!(
            worker_harness.shared().agreement.status(),
            AgreementStatus::Accepted
        );

        let view = coordinator_harness.view(Viewport {
            width: 96,
            color: ColorDepth::Mono,
        });
        for slot in [Slot::Header, Slot::Agents, Slot::State, Slot::StatusBar] {
            assert!(view.slots.contains_key(&slot), "missing {slot:?} view slot");
        }
        assert!(view.slots[&Slot::State].contains("P1 @ 7"));
        assert!(view.slots[&Slot::StatusBar].contains("accepted"));
    }

    #[test]
    fn allocation_plan_borsh_vector_is_stable() {
        let plan = AssignmentPlan {
            assignments: vec![
                Assignment {
                    task: 0,
                    award: Award::Assigned {
                        worker: Participant::new(2),
                        cost: 7,
                    },
                },
                Assignment {
                    task: 1,
                    award: Award::Unassigned,
                },
            ],
        };
        assert_eq!(
            borsh::to_vec(&plan).unwrap(),
            vec![2, 0, 0, 0, 0, 0, 0, 2, 7, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1]
        );
    }
}
