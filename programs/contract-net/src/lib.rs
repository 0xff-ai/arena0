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

    fn initialize(ctx: &mut SharedContext, params: Params) -> Result<(), ProgramFault> {
        validate_params(&params).map_err(|error| anyhow!(error))?;
        ctx.mutate_shared(|state| {
            state.target_size = params.target_size;
            state.tasks = params.tasks;
        });
        Ok(())
    }

    fn on_session_started(
        ctx: &mut SharedContext,
        ensemble: &Ensemble,
    ) -> Result<Transition<Phase>, ProgramFault> {
        if ensemble.len() != ctx.shared().target_size as usize {
            return Err(anyhow!(
                "expected {} participants, got {}",
                ctx.shared().target_size,
                ensemble.len()
            )
            .into());
        }
        ctx.mutate_shared(|state| state.offers = vec![None; ensemble.len()]);
        Ok(Transition::Stay)
    }

    fn on_react(ctx: &mut Context) -> Result<(), ProgramFault> {
        if ctx.shared().expected_writer() != Some(ctx.me()) {
            return Ok(());
        }

        match ctx.shared().phase() {
            Phase::CollectingOffers if ctx.me() == COORDINATOR => {
                if !ctx.local().proposal_sent {
                    let plan = ctx.shared().plan();
                    ctx.effects().broadcast(&Message::Proposal { plan });
                    ctx.mutate_local(|local| local.proposal_sent = true);
                }
            }
            Phase::CollectingOffers => {
                if !ctx.local().offer_sent {
                    let tasks = ctx.shared().tasks.clone();
                    let maximum_capacity = tasks.len() as u16;
                    ctx.effects()
                        .callout(callouts::SubmitOffer {
                            tasks,
                            maximum_capacity,
                            maximum_cost: MAX_COST,
                        })
                        .dispatch();
                }
            }
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
                    ctx.effects().broadcast(&Message::Accept {
                        proposal: proposal_id,
                    });
                    ctx.mutate_local(|local| local.accepted_proposal = Some(proposal_id));
                }
            }
        }
        Ok(())
    }

    fn on_input(ctx: &mut Context, input: Input) -> Result<(), InputFault> {
        let Input::SubmitOffer(offer) = input;
        if ctx.shared().phase() != Phase::CollectingOffers
            || ctx.shared().expected_offer_writer() != Some(ctx.me())
        {
            return Err(anyhow!("offer is not due from this participant").into());
        }
        validate_offer(&ctx.shared().tasks, &offer)
            .map_err(|error| anyhow!(error))
            .retryable()?;
        ctx.effects().broadcast(&Message::Offer(offer));
        ctx.mutate_local(|local| local.offer_sent = true);
        Ok(())
    }

    fn on_message(
        ctx: &mut SharedContext,
        from: Participant,
        message: Message,
    ) -> Result<ApplyDecision<Phase>, ProtocolFault> {
        if ctx.shared().expected_writer() != Some(from) {
            return Ok(ApplyDecision::Reject);
        }

        match message {
            Message::Offer(offer) => {
                if ctx.shared().phase() != Phase::CollectingOffers
                    || from == COORDINATOR
                    || validate_offer(&ctx.shared().tasks, &offer).is_err()
                {
                    return Ok(ApplyDecision::Reject);
                }
                ctx.mutate_shared(|state| state.offers[from.index()] = Some(offer));
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

                let eligible = (0..ctx.ensemble().len())
                    .map(|index| {
                        Participant::try_from(index)
                            .expect("validated participant count fits a participant index")
                    })
                    .collect();
                let threshold = ctx.ensemble().len() as u16;
                let proposed = ctx.mutate_shared(|state| {
                    state
                        .agreement
                        .propose(PROPOSAL_VERSION, plan, eligible, threshold)
                });
                if proposed.is_err() {
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
                let tally =
                    ctx.mutate_shared(|state| state.agreement.vote(from, proposal, Vote::Accept));
                let Ok(tally) = tally else {
                    return Ok(ApplyDecision::Reject);
                };
                match tally {
                    Tally::Pending { .. } => Ok(ApplyDecision::Accept(Transition::Stay)),
                    Tally::Accepted { .. } => Ok(ApplyDecision::Accept(Transition::End)),
                    Tally::NotStarted | Tally::Rejected { .. } => {
                        Err(ProtocolFault::shared_violation(anyhow!(
                            "accept-only ballot reached an impossible result"
                        )))
                    }
                }
            }
        }
    }

    fn view(ctx: &SharedContext, vp: &Viewport) -> View {
        let state = ctx.shared();
        View::new()
            .header(vp.fit_text(format!(
                "Contract net - {} task{}",
                state.tasks.len(),
                if state.tasks.len() == 1 { "" } else { "s" }
            )))
            .agents(vp.fit_text(render_agents(ctx)))
            .state(vp.fit_text(render_state(state)))
            .status_bar(vp.fit_text(render_status(state)))
    }

    fn on_query(_ctx: &SharedContext, _: ()) {}

    fn render_agents(ctx: &SharedContext) -> String {
        let mut output = String::new();
        for index in 0..ctx.ensemble().len() {
            let participant = Participant::try_from(index)
                .expect("validated participant count fits a participant index");
            if participant == COORDINATOR {
                let _ = writeln!(output, "P{index}: coordinator");
                continue;
            }
            let offer = ctx.shared().offers.get(index).and_then(Option::as_ref);
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
}

fn validate_params(params: &Params) -> Result<(), String> {
    if !(2..=MAX_PARTICIPANTS).contains(&params.target_size) {
        return Err(format!(
            "target_size must be between 2 and {MAX_PARTICIPANTS}"
        ));
    }
    if params.tasks.is_empty() || params.tasks.len() > MAX_TASKS {
        return Err(format!("task count must be between 1 and {MAX_TASKS}"));
    }
    for (index, task) in params.tasks.iter().enumerate() {
        validate_text("task name", &task.name)?;
        validate_text("task capability", &task.capability)?;
        if params.tasks[..index]
            .iter()
            .any(|prior| prior.name == task.name)
        {
            return Err(format!("duplicate task name {:?}", task.name));
        }
    }
    Ok(())
}

fn validate_offer(tasks: &[Task], offer: &WorkerOffer) -> Result<(), String> {
    if usize::from(offer.capacity) > tasks.len() {
        return Err("worker capacity exceeds the task count".to_string());
    }
    if offer.capabilities.len() > MAX_TASKS {
        return Err("worker declares too many capabilities".to_string());
    }
    for (index, capability) in offer.capabilities.iter().enumerate() {
        validate_text("worker capability", capability)?;
        if offer.capabilities[..index].contains(capability) {
            return Err(format!("duplicate worker capability {capability:?}"));
        }
    }
    if offer.bids.len() > tasks.len() {
        return Err("worker declares too many bids".to_string());
    }
    for (index, bid) in offer.bids.iter().enumerate() {
        let Some(task) = tasks.get(usize::from(bid.task)) else {
            return Err(format!("bid references unknown task {}", bid.task));
        };
        if bid.cost > MAX_COST {
            return Err(format!("bid cost exceeds {MAX_COST}"));
        }
        if !offer.capabilities.contains(&task.capability) {
            return Err(format!(
                "bid for task {} lacks capability {:?}",
                bid.task, task.capability
            ));
        }
        if offer.bids[..index]
            .iter()
            .any(|prior| prior.task == bid.task)
        {
            return Err(format!("duplicate bid for task {}", bid.task));
        }
    }
    Ok(())
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
    use arena0::testing::{FaultStatus, Harness};
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
        let tasks = tasks();
        assert!(validate_offer(&tasks, &offer(&["design"], 1, &[(0, 4)])).is_err());
        assert!(validate_offer(&tasks, &offer(&["rust"], 1, &[(0, 4), (0, 5)])).is_err());
        assert!(validate_offer(&tasks, &offer(&["rust"], 4, &[(0, 4)])).is_err());
    }

    #[arena0::test(
        ContractNet,
        Params {
            target_size: 2,
            tasks: vec![Task {
                name: "compile".to_string(),
                capability: "rust".to_string(),
            }],
        }
    )]
    fn native_scenario_accepts_only_the_exact_plan(h: _) {
        let worker = PeerId([1; 32]);
        let coordinator = h.peer_id();
        let started = h.session_started(worker);
        assert!(matches!(started.fault, FaultStatus::None));

        let offered = h.message(worker, Message::Offer(offer(&["rust"], 1, &[(0, 7)])));
        let proposal = offered
            .messages::<Message>()
            .into_iter()
            .find(|message| matches!(message, Message::Proposal { .. }))
            .expect("coordinator emits the deterministic proposal");
        let proposed = h.message(coordinator, proposal);
        assert!(matches!(proposed.fault, FaultStatus::None));

        let wrong = ProposalId {
            version: PROPOSAL_VERSION,
            hash: [0xff; 32],
        };
        assert!(
            h.message(coordinator, Message::Accept { proposal: wrong })
                .rejected
        );

        let id = h
            .shared()
            .agreement
            .proposal()
            .expect("proposal is active")
            .id;
        let first = h.message(coordinator, Message::Accept { proposal: id });
        assert!(matches!(first.fault, FaultStatus::None));
        let finished = h.message(worker, Message::Accept { proposal: id });
        assert!(matches!(finished.fault, FaultStatus::None));
        assert!(
            finished
                .step
                .as_ref()
                .is_some_and(|step| step.is_terminal())
        );
        assert_eq!(h.shared().agreement.status(), AgreementStatus::Accepted);

        let view = h.view(Viewport {
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
