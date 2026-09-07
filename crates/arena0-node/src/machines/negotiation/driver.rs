//! The negotiation driver: one local node through the negotiation flow
//! (`docs/protocol-architecture.md` §7–8) until it durably commits the activation
//! or gives up. The local node is either the **creator** (it made the offer)
//! or a **participant**. Everything is periodic broadcast on the program
//! topic — no pull, no direct streams — with one bounded exception: the
//! convergence fetch (a participating peer pulls any missed tickets from the
//! creator before producing its activation signature).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use arena0_crypto::{BlsSignature, ExecutionKey, NodeKeys};
use arena0_protocol::{
    Activation, ActivationAnnouncement, ActivationData, ActivationSignature, ActivationTickets,
    Counteroffer, EventSource, ExecId, FetchActivationTickets, FetchFrame, MAX_CLOCK_SKEW_MS,
    NegotiationEvent, NegotiationFact, NegotiationGossip, NegotiationStage, Offer, OfferData,
    OfferHash, PREPARE_WINDOW_MS, PeerId, PeerIdSource, PreparedActivation, SessionHash, Ticket,
    TicketAction, TicketData, TicketHash,
};
use arena0_store::ExecutionStore;
use arena0_transport::{NegotiationTopic, ProgramTopicEvent, RecvHandle, Transport};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout_at};

use crate::machines::activation::ActivatedSession;
use crate::router::FetchRegistry;

use super::support::{
    DurableOutcome, FETCH_TIMEOUT, LocalTicketWithdrawal, NegotiationAttempt,
    NegotiationDriveError, NegotiationEffects, NegotiationStart, NegotiationSupervision,
    PersistActivationEffect, PrepareEffect, PrepareOutcome, RecomputeInitialStateEffect,
    serve_fetch_evidence, unix_time_ms,
};
use super::{ApplyError, ApplyOutcome, NegotiationBook};

/// Negotiation broadcast cadence: 2 s ± 50 % jitter (tunable).
const CADENCE_MS: u64 = 2_000;
/// Maximum creator re-offer attempts before giving up (tunable).
const REOFFER_MAX_ATTEMPTS: u32 = 8;
/// Deadline window of a re-offer, in milliseconds (tunable).
const REOFFER_WINDOW_MS: u64 = 30_000;
/// Once a participant set has formed, bound convergence and activation.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);

fn ticket_prepare_window_ok(ticket: &Ticket, now: u64) -> bool {
    let TicketAction::Active {
        issued_at_unix_ms,
        valid_for_ms,
        ..
    } = &ticket.data.action
    else {
        return false;
    };
    *issued_at_unix_ms <= now.saturating_add(MAX_CLOCK_SKEW_MS)
        && issued_at_unix_ms
            .saturating_add(u64::from(*valid_for_ms))
            .saturating_sub(now)
            >= MAX_CLOCK_SKEW_MS + PREPARE_WINDOW_MS
}

/// Role-specific state for one negotiation drive.
///
/// A drive keeps one role for its lifetime: a creator may issue re-offers and
/// aggregate signatures, while a participant may accept re-offers and prepare
/// from the creator's frozen evidence. Keeping those state machines separate
/// makes the role-specific invariants explicit in the type.
#[derive(Debug)]
enum NegotiationRole {
    Creator(CreatorState),
    Participant(ParticipantState),
}

enum SupervisionEvent {
    Withdrawal(Option<Box<LocalTicketWithdrawal>>),
    Requested(bool),
}

#[derive(Debug, Default)]
struct CreatorState {
    frozen: Option<PreparedActivation>,
    activation_signatures: HashMap<TicketHash, BlsSignature>,
    reoffer_attempts: u32,
    /// Every params the creator has offered, with the counteroffer support
    /// observed when it was last offered. A failed params is never
    /// auto-retried unless its support strictly increases.
    tried_params: HashMap<Vec<u8>, usize>,
}

#[derive(Debug, Default)]
struct ParticipantState {
    /// An accepted re-offer (higher offer_seq, acceptance policy passed)
    /// waiting for the creator's Active ticket to authenticate it.
    pending_reoffer: Option<Offer>,
    prepared: Option<PreparedActivation>,
    fetch_sent_at: Option<Instant>,
}

impl NegotiationRole {
    fn creator_state(&self) -> Option<&CreatorState> {
        match self {
            Self::Creator(state) => Some(state),
            Self::Participant(_) => None,
        }
    }

    fn creator_state_mut(&mut self) -> Option<&mut CreatorState> {
        match self {
            Self::Creator(state) => Some(state),
            Self::Participant(_) => None,
        }
    }

    fn participant_state(&self) -> Option<&ParticipantState> {
        match self {
            Self::Creator(_) => None,
            Self::Participant(state) => Some(state),
        }
    }

    fn participant_state_mut(&mut self) -> Option<&mut ParticipantState> {
        match self {
            Self::Creator(_) => None,
            Self::Participant(state) => Some(state),
        }
    }

    fn prepared_activation(&self) -> Option<&PreparedActivation> {
        match self {
            Self::Creator(state) => state.frozen.as_ref(),
            Self::Participant(state) => state.prepared.as_ref(),
        }
    }
}

/// Drive one local node through negotiation and durable activation until it
/// durably commits the activation or gives up.
pub(crate) struct NegotiationDriver<'store, 'effects> {
    topic: Box<dyn NegotiationTopic>,
    transport: std::sync::Arc<dyn Transport + Sync>,
    identity: &'store NodeKeys,
    execution: &'store ExecutionKey,
    execution_store: &'store mut ExecutionStore,
    exec_id: ExecId,
    offer: Offer,
    /// The local Active ticket, signed only after the offer is authenticated
    /// (the creator's Active ticket for the offer verifies in the book). The
    /// creator signs immediately; a participant signs when the creator's
    /// ticket authenticates the offer.
    local_ticket: Option<Ticket>,
    preferred_params: Option<Vec<u8>>,
    book: NegotiationBook<'store>,
    prepare: PrepareEffect,
    persist_activation: PersistActivationEffect,
    recompute_initial_state: RecomputeInitialStateEffect,
    event_sink: &'effects (dyn Fn(EventSource, NegotiationEvent) + Send + Sync),
    supervision: Option<NegotiationSupervision>,
    /// The session-keyed fetch registry the accept router routes fetch
    /// streams through; the driver registers its expected session hash and
    /// unregisters on drop.
    fetch_registry: FetchRegistry,
    fetch_rx: mpsc::Receiver<(RecvHandle, FetchFrame)>,
    fetch_rx_closed: bool,
    /// The session hash this drive is currently registered for.
    registered_hash: Option<SessionHash>,
    deadline: Option<Instant>,
    neighbors: HashSet<PeerId>,
    role: NegotiationRole,
    emitted_signature: bool,
    /// Prevent duplicate timeout facts when several deadline paths race to
    /// report the same terminal timeout.
    timed_out_emitted: bool,
    /// The durable session restored at construction, emitted once on run.
    resumed: Option<SessionHash>,
    cadence_counter: u64,
}

impl std::fmt::Debug for NegotiationDriver<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NegotiationDriver")
            .field("negotiation_id", &self.offer.data().negotiation_id)
            .field("offer_seq", &self.offer.data().offer_seq)
            .field("creator", &self.offer.data().creator)
            .field("local_peer", &self.identity.peer_id())
            .field(
                "frozen",
                &matches!(&self.role, NegotiationRole::Creator(state) if state.frozen.is_some()),
            )
            .field(
                "prepared",
                &matches!(&self.role, NegotiationRole::Participant(state) if state.prepared.is_some()),
            )
            .finish_non_exhaustive()
    }
}

impl Drop for NegotiationDriver<'_, '_> {
    fn drop(&mut self) {
        if let Some(hash) = self.registered_hash.take() {
            self.fetch_registry.lock().unwrap().remove(&hash);
        }
    }
}

impl<'store, 'effects> NegotiationDriver<'store, 'effects> {
    pub(crate) fn new(
        transport: std::sync::Arc<dyn Transport + Sync>,
        fetch_registry: FetchRegistry,
        identity: &'store NodeKeys,
        execution: &'store ExecutionKey,
        execution_store: &'store mut ExecutionStore,
        attempt: NegotiationAttempt,
        effects: NegotiationEffects<'effects>,
    ) -> Result<Self, NegotiationDriveError> {
        let NegotiationAttempt {
            topic,
            exec_id,
            start,
            supervision,
            deadline,
        } = attempt;
        let (offer, preferred_params, creator_ticket, resume) = match start {
            NegotiationStart::Fresh {
                offer,
                creator_ticket,
                preferred_params,
            } => (offer, preferred_params, creator_ticket, None),
            NegotiationStart::Resume {
                local_ticket,
                activation,
            } => {
                let activation = *activation;
                let offer = activation.offer().clone();
                (offer, None, None, Some((local_ticket, activation)))
            }
        };
        let NegotiationEffects {
            prepare,
            persist_activation,
            recompute_initial_state,
            emit: event_sink,
        } = effects;
        let local_profile = arena0_program::ExecutionProfile::current().hash();
        offer
            .data()
            .validate_for_profile(local_profile)
            .map_err(|error| {
                if let arena0_protocol::NegotiationError::ExecutionProfileMismatch {
                    offered,
                    local,
                } = error
                {
                    NegotiationDriveError::ExecutionProfileMismatch { offered, local }
                } else {
                    NegotiationDriveError::InvalidLocalTicket
                }
            })?;
        let mut book = NegotiationBook::new(identity, execution);
        book.register_offer(offer.data().negotiation_id, offer.data().offer_seq, &offer)
            .map_err(|_| NegotiationDriveError::InvalidLocalTicket)?;
        let role = if offer.data().creator == identity.peer_id() {
            let mut tried_params = HashMap::new();
            tried_params.insert(offer.data().params.as_bytes().to_vec(), 0);
            NegotiationRole::Creator(CreatorState {
                tried_params,
                ..CreatorState::default()
            })
        } else {
            NegotiationRole::Participant(ParticipantState::default())
        };
        let mut driver = Self {
            topic,
            transport,
            identity,
            execution,
            execution_store,
            exec_id,
            offer: offer.clone(),
            local_ticket: None,
            preferred_params,
            book,
            prepare,
            persist_activation,
            recompute_initial_state,
            event_sink,
            supervision,
            fetch_registry,
            fetch_rx: mpsc::channel(1).1,
            fetch_rx_closed: false,
            registered_hash: None,
            deadline,
            neighbors: HashSet::new(),
            role,
            emitted_signature: false,
            timed_out_emitted: false,
            resumed: None,
            cadence_counter: 0,
        };
        // A resumed drive restores the exact local ticket and the prepared
        // (or frozen) evidence from the ActivationRecord: it never re-signs
        // a fresh ticket, so the TicketHash matches the durable prepare.
        if let Some((local_ticket, activation)) = resume {
            let session_hash = activation.session_hash();
            driver.resumed = Some(session_hash);
            if let Some(supervision) = &driver.supervision {
                let _ = supervision.ticket.send(Some(local_ticket.clone()));
            }
            driver.local_ticket = Some(local_ticket);
            // Restore the durable activation's tickets into the book: the
            // driver's bootstrap set (and the periodic join) reads the book,
            // and a resumed drive must know its committed signers. A ticket
            // that fails the book's checks is an internal recovery failure:
            // the durable record is inconsistent with the wire model.
            for ticket in activation.tickets() {
                driver
                    .book
                    .apply_ticket(ticket)
                    .map_err(|error| NegotiationDriveError::Prepare(error.to_string()))?;
            }
            match &mut driver.role {
                NegotiationRole::Creator(state) => state.frozen = Some(activation.clone()),
                NegotiationRole::Participant(state) => state.prepared = Some(activation),
            }
            driver.emitted_signature = false;
        }
        if driver.local_ticket.is_none() {
            // The creator ticket is formed from the same OfferData before the
            // Offer crosses this boundary. Re-issuing it here would change
            // its timestamp and therefore its TicketHash.
            let creator = matches!(&driver.role, NegotiationRole::Creator(_));
            if let Some(ticket) = creator_ticket.as_ref() {
                if ticket.data.signer
                    != if creator {
                        identity.peer_id()
                    } else {
                        offer.data().creator
                    }
                    || ticket.data.negotiation_id != offer.data().negotiation_id
                    || ticket.data.offer_seq != offer.data().offer_seq
                    || TicketHash::of(&ticket.data) != offer.tickets()[0]
                {
                    return Err(NegotiationDriveError::InvalidLocalTicket);
                }
                driver
                    .book
                    .apply_ticket(ticket)
                    .map_err(|_| NegotiationDriveError::InvalidLocalTicket)?;
                if creator {
                    if let Some(supervision) = &driver.supervision {
                        let _ = supervision.ticket.send(Some(ticket.clone()));
                    }
                    driver.local_ticket = Some(ticket.clone());
                }
            } else if creator {
                return Err(NegotiationDriveError::InvalidLocalTicket);
            }
        }
        driver.refresh_fetch_registration();
        Ok(driver)
    }

    /// Sign the local Active ticket for the current offer (revision 0)
    /// through the book and push it to the daemon's slot. The book gates
    /// issuance on the offer's authentication: the local peer is the creator,
    /// or the creator's Active ticket is already applied. The ticket consents
    /// to the exact offer: its scope-bound `key_binding` commits to the
    /// `OfferHash`.
    fn issue_local_ticket(&mut self) -> Result<Ticket, ApplyError> {
        let ticket = self.book.issue_ticket(
            self.offer.data().negotiation_id,
            self.offer.data().offer_seq,
            0,
            unix_time_ms(),
        )?;
        if let Some(supervision) = &self.supervision {
            let _ = supervision.ticket.send(Some(ticket.clone()));
        }
        self.local_ticket = Some(ticket.clone());
        Ok(ticket)
    }

    /// (Re)register the convergence-fetch handler for the current expected
    /// session hash. The accept router routes fetch streams by session hash,
    /// so the registration follows the expected hash as it changes (freeze,
    /// frozen offer, re-offer, switch).
    fn refresh_fetch_registration(&mut self) {
        let expected = self.expected_session_hash().ok();
        if expected == self.registered_hash {
            return;
        }
        if let Some(old) = self.registered_hash.take() {
            self.fetch_registry.lock().unwrap().remove(&old);
        }
        if let Some(hash) = expected {
            let (tx, rx) = mpsc::channel(64);
            self.fetch_registry.lock().unwrap().insert(hash, tx);
            self.fetch_rx = rx;
            self.fetch_rx_closed = false;
            self.registered_hash = Some(hash);
        }
    }

    /// A participant signs its ticket only after the offer is authenticated:
    /// the creator's Active ticket for the offer verifies in the book (the
    /// book checked its ed25519 identity signature and the scope-bound
    /// `key_binding` over the offer hash). The creator signs at construction.
    async fn ensure_local_ticket(&mut self) -> Result<(), NegotiationDriveError> {
        if self.local_ticket.is_some() {
            return Ok(());
        }
        let NegotiationRole::Participant(_) = &self.role else {
            return Ok(());
        };
        // The acceptance policy applies to every offer the participant
        // tickets: the params must match the local preference (when one is
        // set). A mismatch means the participant counters instead of
        // ticketing — the creator tunes the re-offer to the plurality.
        if let Some(params) = &self.preferred_params
            && self.offer.data().params.as_bytes() != params.as_slice()
        {
            return Ok(());
        }
        // The offer's initial state must match the local computation for
        // those params; a mismatch means the participant counters instead.
        match (self.recompute_initial_state)(self.offer.data().params.as_bytes()) {
            Ok(initial_state) if initial_state == self.offer.data().initial_state => {}
            _ => return Ok(()),
        }
        // The book issues the ticket only after the creator's Active ticket
        // authenticates the offer; an unauthenticated offer means the
        // creator's ticket has not arrived yet — wait for the next cadence.
        match self.issue_local_ticket() {
            Ok(ticket) => {
                self.publish(NegotiationFact::Ticket(ticket)).await?;
                Ok(())
            }
            Err(ApplyError::UnauthenticatedOffer { .. }) => Ok(()),
            Err(_) => Err(NegotiationDriveError::InvalidLocalTicket),
        }
    }

    /// Run until this node durably commits the activation and return the
    /// confirmed session.
    pub(crate) async fn run(mut self) -> Result<ActivatedSession, NegotiationDriveError> {
        if self.withdrawal_requested() && self.on_withdrawal_requested().await? {
            return Err(NegotiationDriveError::Withdrawn);
        }
        self.emit_negotiation_event(NegotiationEvent::Started {
            target_size: self.offer.data().target_size,
        });
        if let Some(session_hash) = self.resumed {
            self.emit_negotiation_event(NegotiationEvent::PreparedActivationResumed {
                session_hash,
                participant_count: bounded_count(self.offer.data().target_size as usize),
            });
        }
        self.emit_negotiation_event(NegotiationEvent::OfferAccepted {
            creator: self.offer.data().creator,
            offer_seq: self.offer.data().offer_seq,
        });
        if let Some(activated) = self.emit_periodic().await? {
            return Ok(activated);
        }
        let mut cadence = Box::pin(sleep_until(self.next_cadence()));

        loop {
            tokio::select! {
                biased;
                () = wait_deadline(self.deadline) => return Err(self.timeout_error()),
                event = self.topic.recv() => {
                    match event {
                        Ok(event) => {
                            if let Some(activated) = self.on_topic_event(event).await? {
                                return Ok(activated);
                            }
                        }
                        Err(_) => self.rejoin_topic().await?,
                    }
                }
                local = async {
                    match &mut self.supervision {
                        Some(supervision) => {
                            tokio::select! {
                                withdrawal = supervision.withdrawals.recv() => {
                                    SupervisionEvent::Withdrawal(withdrawal.map(Box::new))
                                }
                                changed = supervision.withdrawal_requested.changed() => {
                                    SupervisionEvent::Requested(changed.is_ok())
                                }
                            }
                        }
                        None => std::future::pending().await,
                    }
                } => match local {
                    SupervisionEvent::Withdrawal(Some(withdrawal)) => {
                        if self.on_local_withdrawal(*withdrawal).await? {
                            return Err(NegotiationDriveError::Withdrawn);
                        }
                    }
                    SupervisionEvent::Withdrawal(None) => self.supervision = None,
                    SupervisionEvent::Requested(true)
                        if self.withdrawal_requested()
                            && self.on_withdrawal_requested().await? =>
                    {
                        return Err(NegotiationDriveError::Withdrawn);
                    }
                    SupervisionEvent::Requested(_) => {}
                },
                fetch = async {
                    if self.fetch_rx_closed {
                        std::future::pending().await
                    } else {
                        self.fetch_rx.recv().await
                    }
                } => {
                    let Some((recv, frame)) = fetch else {
                        self.fetch_rx_closed = true;
                        continue;
                    };
                    if let Some(activated) = self.on_fetch_stream(recv, frame).await? {
                        return Ok(activated);
                    }
                }
                () = &mut cadence => {
                    if let Some(activated) = self.emit_periodic().await? {
                        return Ok(activated);
                    }
                    cadence.as_mut().reset(self.next_cadence());
                }
            }
        }
    }

    // ── Topic events ────────────────────────────────────────────────────

    async fn on_topic_event(
        &mut self,
        event: ProgramTopicEvent,
    ) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        let fact = match event {
            ProgramTopicEvent::Joined => return Ok(None),
            ProgramTopicEvent::NeighborUp(peer) => {
                self.neighbors.insert(peer);
                return Ok(None);
            }
            ProgramTopicEvent::NeighborDown(peer) => {
                self.neighbors.remove(&peer);
                return Ok(None);
            }
            ProgramTopicEvent::Lagged | ProgramTopicEvent::Closed => {
                self.rejoin_topic().await?;
                return Ok(None);
            }
            ProgramTopicEvent::Fact(fact) => fact,
        };
        let Ok(frame) = NegotiationGossip::decode(&fact.bytes) else {
            return Ok(None);
        };
        if frame.program_id != self.offer.data().program_hash
            || frame.negotiation_id != self.offer.data().negotiation_id
        {
            return Ok(None);
        }
        self.on_fact(frame.fact).await
    }

    async fn on_fact(
        &mut self,
        fact: NegotiationFact,
    ) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        match fact {
            NegotiationFact::Offer(offer) => self.on_offer(offer).await,
            NegotiationFact::Ticket(ticket) => {
                self.on_ticket(ticket).await?;
                self.after_ticket().await
            }
            NegotiationFact::ActivationSignature(signature) => {
                self.on_activation_signature(signature)?;
                self.after_signature().await
            }
            NegotiationFact::ActivationAnnouncement(announcement) => {
                self.on_announcement(announcement).await
            }
            NegotiationFact::Counteroffer(counteroffer) => {
                self.on_counteroffer(counteroffer)?;
                Ok(None)
            }
        }
    }

    async fn on_offer(
        &mut self,
        offer: Offer,
    ) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        // The creator makes the offers; it ignores all offers on the topic.
        if !matches!(&self.role, NegotiationRole::Participant(_)) {
            return Ok(None);
        }
        // A different offer_seq is a re-offer: the old offer died by
        // reference. A prepared signer is bound to the old SessionHash and
        // stays with it; an unprepared participant accepts the re-offer only
        // when it passes the acceptance policy (monotonic sequence, same
        // creator, params matching the local preference, and a consistent
        // initial state) and is authenticated through the creator's Active
        // ticket for the new offer_seq.
        if offer.data().offer_seq != self.offer.data().offer_seq {
            if matches!(&self.role, NegotiationRole::Participant(state) if state.prepared.is_some())
            {
                return Ok(None);
            }
            if !self.accept_reoffer(&offer) {
                return Ok(None);
            }
            // The newest offer for the slot wins the pending slot; the
            // creator's Active ticket (bound to the real OfferHash)
            // authenticates it before any switch. A counterfeit offer is
            // never registered, so it cannot poison the slot.
            if let NegotiationRole::Participant(state) = &mut self.role {
                state.pending_reoffer = Some(offer);
            }
            return Ok(None);
        }
        // The same offer_seq must carry the same content (same OfferHash);
        // an altered offer does not verify against the tickets' key_bindings.
        if OfferHash::of(offer.data()) != OfferHash::of(self.offer.data()) {
            return Ok(None);
        }
        // Track the creator's view of the forming ticket set. The immutable
        // offer carries no activation announcement; a complete offer is frozen only when
        // it is paired with exact durable ticket bodies.
        let updated_offer = match Offer::new(offer.data().clone(), offer.tickets().to_vec()) {
            Ok(offer) => offer,
            Err(_) => return Ok(None),
        };
        self.offer = updated_offer;
        self.refresh_fetch_registration();
        // The frozen offer: fetch any missed tickets and prepare.
        if offer.tickets().len() == self.offer.data().target_size as usize {
            self.try_fetch_and_prepare().await?;
        }
        Ok(None)
    }

    async fn on_ticket(&mut self, ticket: Ticket) -> Result<(), NegotiationDriveError> {
        if ticket.data.negotiation_id != self.offer.data().negotiation_id {
            return Ok(());
        }
        if ticket.data.offer_seq > self.offer.data().offer_seq {
            // A re-offer ticket: authenticate the pending re-offer through
            // the creator's Active ticket — its ed25519 identity signature
            // and its scope-bound key_binding over the pending offer's hash.
            // A counterfeit offer never verifies, so it is inert.
            let Some(offer) = (match &self.role {
                NegotiationRole::Participant(state) => state.pending_reoffer.clone(),
                NegotiationRole::Creator(_) => None,
            }) else {
                return Ok(());
            };
            if offer.data().offer_seq != ticket.data.offer_seq
                || ticket.data.signer != offer.data().creator
            {
                return Ok(());
            }
            if !matches!(ticket.data.action, TicketAction::Active { .. }) {
                return Ok(());
            }
            if ticket
                .verify_for_offer(&OfferHash::of(offer.data()))
                .is_err()
            {
                return Ok(());
            }
            if let NegotiationRole::Participant(state) = &mut self.role {
                state.pending_reoffer = None;
            }
            self.switch_offer(offer, ticket)?;
            return Ok(());
        }
        if ticket.data.offer_seq != self.offer.data().offer_seq {
            return Ok(());
        }
        // The book validates the identity signature and the scope-bound
        // key_binding against the tracked offer hash; malformed, stale, and
        // duplicate tickets are inert.
        if let Ok(outcome) = self.book.apply_ticket(&ticket)
            && matches!(outcome, ApplyOutcome::Inserted | ApplyOutcome::Replaced)
        {
            self.emit_ticket_accepted(&ticket);
            // The creator's Active ticket authenticates the offer: only then
            // does the participant sign its own consent ticket.
            if ticket.data.signer == self.offer.data().creator {
                self.ensure_local_ticket().await?;
            }
        }
        Ok(())
    }

    fn on_activation_signature(
        &mut self,
        signature: ActivationSignature,
    ) -> Result<(), NegotiationDriveError> {
        if !matches!(&self.role, NegotiationRole::Creator(_)) || signature.validate().is_err() {
            return Ok(());
        }
        let Some(frozen) = (match &self.role {
            NegotiationRole::Creator(state) => state.frozen.clone(),
            NegotiationRole::Participant(_) => None,
        }) else {
            return Ok(());
        };
        if signature.session_hash != frozen.session_hash() {
            return Ok(());
        }
        let Some(execution_bls) = frozen
            .tickets()
            .iter()
            .find(|ticket| TicketHash::of(&ticket.data) == signature.ticket_hash)
            .and_then(|ticket| match &ticket.data.action {
                TicketAction::Active { execution_bls, .. } => Some(*execution_bls),
                TicketAction::Withdrawn => None,
            })
        else {
            return Ok(());
        };
        let activation_data = frozen.activation_data().clone();
        // A malformed signature is inert untrusted input, not a fatal error.
        if !execution_bls
            .verify(&activation_data.signing_bytes(), &signature.signature)
            .unwrap_or(false)
        {
            return Ok(());
        }
        if let NegotiationRole::Creator(state) = &mut self.role {
            state
                .activation_signatures
                .insert(signature.ticket_hash, signature.signature);
        }
        Ok(())
    }

    fn on_counteroffer(&mut self, counteroffer: Counteroffer) -> Result<(), NegotiationDriveError> {
        // The book validates the identity signature, the time bounds, and the
        // live-known-negotiation rule; invalid counteroffers are inert.
        let _ = self.book.apply_counteroffer(&counteroffer, unix_time_ms());
        Ok(())
    }

    async fn after_ticket(&mut self) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        match &self.role {
            NegotiationRole::Creator(_) => {
                self.try_freeze().await?;
                self.try_aggregate().await
            }
            NegotiationRole::Participant(_) => {
                self.try_fetch_and_prepare().await?;
                Ok(None)
            }
        }
    }

    async fn after_signature(&mut self) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        match &self.role {
            NegotiationRole::Creator(_) => self.try_aggregate().await,
            NegotiationRole::Participant(_) => Ok(None),
        }
    }

    // ── Periodic emit ───────────────────────────────────────────────────

    async fn emit_periodic(&mut self) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        self.cadence_counter = self.cadence_counter.wrapping_add(1);
        self.emit_retry();
        // Re-issue the bootstrap join only while the topic has no neighbors:
        // the subscribe-time join is one-shot and fails silently when a
        // peer is down or its addresses are not yet known (e.g. a resumed
        // drive before address introduction). Once a neighbor is up, stop
        // joining — repeated high-priority joins churn the gossip view.
        if self.neighbors.is_empty() {
            let bootstrap = self.bootstrap_peers();
            if !bootstrap.is_empty() {
                let _ = self.topic.join_peers(bootstrap).await;
            }
        }
        match &self.role {
            NegotiationRole::Creator(_) => self.creator_emit().await,
            NegotiationRole::Participant(_) => self.participant_emit().await,
        }
    }

    async fn creator_emit(&mut self) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        // Open admission renews before the offer's remaining prepare window
        // closes. A frozen proposal keeps its exact evidence and completes
        // under the bounded activation deadline instead.
        let renew_at = if self.deadline.is_none() {
            self.offer
                .data()
                .deadline_unix_ms
                .saturating_sub(PREPARE_WINDOW_MS + MAX_CLOCK_SKEW_MS)
        } else {
            self.offer.data().deadline_unix_ms
        };
        if unix_time_ms() >= renew_at
            && self
                .role
                .creator_state()
                .is_some_and(|state| state.frozen.is_none())
        {
            self.try_reoffer()?;
        }
        // Freeze when the ticket set reached target_size.
        self.try_freeze().await?;
        // A resumed drive re-publishes the creator's activation signature
        // and records it locally: the topic does not echo a publisher's
        // facts, so the aggregate must include it.
        if self
            .role
            .creator_state()
            .is_some_and(|state| state.frozen.is_some())
            && !self.emitted_signature
        {
            let frame = self.publish_local_signature().await?;
            if let Some(state) = self.role.creator_state_mut() {
                state
                    .activation_signatures
                    .insert(frame.ticket_hash, frame.signature);
            }
        }
        // Emit the offer and the tickets.
        if let Some(frozen) = self
            .role
            .creator_state()
            .and_then(|state| state.frozen.clone())
        {
            self.publish(NegotiationFact::Offer(frozen.offer().clone()))
                .await?;
            for ticket in frozen.tickets() {
                self.publish(NegotiationFact::Ticket(ticket.clone()))
                    .await?;
            }
        } else {
            let set = self.book.ticket_set(
                self.offer.data().negotiation_id,
                self.offer.data().offer_seq,
            );
            let hashes = set
                .iter()
                .map(|ticket| TicketHash::of(&ticket.data))
                .collect::<Vec<_>>();
            let offer = Offer::new(self.offer.data().clone(), hashes)
                .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
            self.publish(NegotiationFact::Offer(offer)).await?;
            self.publish(NegotiationFact::Ticket(
                self.local_ticket
                    .as_ref()
                    .expect("the creator's ticket is signed at construction")
                    .clone(),
            ))
            .await?;
        }
        // Aggregate when every signer's activation signature is in.
        self.try_aggregate().await
    }

    async fn participant_emit(
        &mut self,
    ) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        // Emit the key certificate (once the offer is authenticated) and the
        // preference announcement.
        self.ensure_local_ticket().await?;
        if let Some(ticket) = &self.local_ticket {
            self.publish(NegotiationFact::Ticket(ticket.clone()))
                .await?;
        }
        if let Some(params) = &self.preferred_params {
            let counteroffer = self
                .book
                .emit_counteroffer(
                    self.offer.data().negotiation_id,
                    params.clone(),
                    unix_time_ms(),
                )
                .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
            self.publish(NegotiationFact::Counteroffer(counteroffer))
                .await?;
        }
        // Desertion: the offer deadline passed. Prepared evidence continues
        // independent of the pre-prepare admission deadline: a prepared
        // signer keeps republishing its activation signature and fetching
        // until the outer deadline. An unprepared participant keeps waiting
        // for the re-offer, also bounded by the outer deadline.
        // The signature is republished every cadence, not once: the topic
        // broadcast only acks queueing, so an emission before the gossip
        // has neighbors reaches zero peers and would otherwise strand the
        // aggregate forever.
        if self
            .role
            .participant_state()
            .is_some_and(|state| state.prepared.is_some())
        {
            self.publish_local_signature().await?;
        }
        // Fetch any missed tickets and prepare when the offer is frozen.
        self.try_fetch_and_prepare().await?;
        Ok(None)
    }

    // ── Creator: freeze, aggregate, re-offer ────────────────────────────

    /// Freeze the ticket set at `target_size`: durably prepare the exact
    /// `SessionHash` (the freeze + Prepared boundary), then emit the full
    /// offer and the creator's activation signature.
    async fn try_freeze(&mut self) -> Result<(), NegotiationDriveError> {
        if !matches!(&self.role, NegotiationRole::Creator(state) if state.frozen.is_none()) {
            return Ok(());
        }
        let now = unix_time_ms();
        if self.offer.data().deadline_unix_ms <= now {
            return Ok(());
        }
        let set = self.book.ticket_set(
            self.offer.data().negotiation_id,
            self.offer.data().offer_seq,
        );
        let set = set
            .into_iter()
            .filter(|ticket| ticket_prepare_window_ok(ticket, now))
            .take(self.offer.data().target_size as usize)
            .collect::<Vec<_>>();
        if set.len() < self.offer.data().target_size as usize {
            return Ok(());
        }
        // A creator may observe more active tickets than the requested
        // ensemble size. Freeze the deterministic creator-first prefix so the
        // prepared activation and its hashes contain exactly target_size
        // participants.
        let hashes = set
            .iter()
            .map(|ticket| TicketHash::of(&ticket.data))
            .collect::<Vec<_>>();
        let frozen_offer = Offer::new(self.offer.data().clone(), hashes)
            .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
        let activation = PreparedActivation::new(frozen_offer.clone(), set.clone())
            .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
        match (self.prepare)(&mut *self.execution_store, activation.clone())
            .await
            .map_err(NegotiationDriveError::Prepare)?
        {
            PrepareOutcome::Accepted => {}
            PrepareOutcome::NotPreparable(_) => return Ok(()),
            PrepareOutcome::Conflict => {
                return Err(NegotiationDriveError::Prepare(
                    "local execution is bound to different preparation facts".into(),
                ));
            }
        }
        self.arm_completion_deadline();
        let session_hash = activation.session_hash();
        if let Some(state) = self.role.creator_state_mut() {
            state.frozen = Some(activation);
        }
        // The creator's own view of the ticket set now matches the frozen
        // offer, so its expected session hash (and the fetch registration)
        // tracks the frozen evidence.
        self.offer = frozen_offer.clone();
        self.emit_negotiation_event(NegotiationEvent::ActivationPrepared {
            session_hash,
            participant_count: bounded_count(self.offer.data().target_size as usize),
        });
        self.refresh_fetch_registration();
        // Emit the full offer, then the creator's activation signature. The
        // creator records its own signature directly: the topic does not
        // echo a publisher's facts, and the aggregate must include it.
        self.publish(NegotiationFact::Offer(frozen_offer)).await?;
        let frame = self.publish_local_signature().await?;
        if let Some(state) = self.role.creator_state_mut() {
            state
                .activation_signatures
                .insert(frame.ticket_hash, frame.signature);
        }
        Ok(())
    }

    /// Aggregate every signer's activation signature into the bounded
    /// activation announcement, durably commit (the Committed boundary),
    /// publish the announcement, and hand the session to the relay.
    async fn try_aggregate(&mut self) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        let (frozen, signatures) = {
            let Some(state) = self.role.creator_state() else {
                return Ok(None);
            };
            let Some(frozen) = state.frozen.clone() else {
                return Ok(None);
            };
            if state.activation_signatures.len() < self.offer.data().target_size as usize {
                return Ok(None);
            }
            // Aggregate in the frozen set's order so the aggregate verifies
            // against the keys in that order.
            let signatures = frozen
                .tickets()
                .iter()
                .map(|ticket| state.activation_signatures[&TicketHash::of(&ticket.data)])
                .collect::<Vec<_>>();
            (frozen, signatures)
        };
        let aggregate = BlsSignature::aggregate(&signatures)
            .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
        let activation = Activation::new(frozen, aggregate)
            .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
        match (self.persist_activation)(&mut *self.execution_store, activation.clone())
            .await
            .map_err(NegotiationDriveError::PersistCommit)?
        {
            DurableOutcome::Accepted => {}
            DurableOutcome::Conflict => {
                return Err(NegotiationDriveError::PersistCommit(
                    "local execution is bound to another activation".into(),
                ));
            }
        }
        let session_hash = activation.session_hash();
        self.emit_negotiation_event(NegotiationEvent::ActivationCommitted {
            session_hash,
            participant_count: bounded_count(self.offer.data().target_size as usize),
        });
        // Publish the session announcement best-effort. The durable commit is
        // the point of no return: the daemon's post-commit relay re-emits
        // the offer and the exact tickets from the committed
        // ActivationRecord while the session is live, so a publication
        // failure here must not strand the committed activation.
        let announcement = ActivationAnnouncement::from_activation(&activation);
        if let Err(error) = self
            .publish(NegotiationFact::ActivationAnnouncement(announcement))
            .await
        {
            tracing::warn!(
                exec_id = %self.exec_id,
                %session_hash,
                %error,
                "announcement publication failed after commit; the relay will re-emit it"
            );
        }
        let activated = ActivatedSession::new(activation)
            .map_err(|error| NegotiationDriveError::PersistCommit(error.to_string()))?;
        Ok(Some(activated))
    }

    /// Prefer a supported counteroffer within the tuning budget. An open
    /// creator otherwise renews the current terms without spending that
    /// budget, preserving its negotiation identity while it waits.
    fn try_reoffer(&mut self) -> Result<(), NegotiationDriveError> {
        if self.role.creator_state().is_none()
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(self.timeout_error());
        }
        let now_ms = unix_time_ms();
        let mut groups: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
        for counteroffer in self
            .book
            .valid_counteroffers(self.offer.data().negotiation_id, now_ms)
        {
            *groups.entry(counteroffer.data.params.clone()).or_insert(0) += 1;
        }
        let candidate = groups
            .into_iter()
            .max_by(
                |(left_params, left_support), (right_params, right_support)| {
                    left_support
                        .cmp(right_support)
                        .then_with(|| right_params.cmp(left_params))
                },
            )
            .filter(|(params, support)| {
                *support + 1 >= self.offer.data().target_size as usize
                    && self.role.creator_state().is_some_and(|state| {
                        state.reoffer_attempts < REOFFER_MAX_ATTEMPTS
                            && state
                                .tried_params
                                .get(params)
                                .is_none_or(|previous| support > previous)
                    })
            });
        let (params, initial_state) = if let Some((params, support)) = candidate {
            let initial_state =
                (self.recompute_initial_state)(&params).map_err(NegotiationDriveError::Prepare)?;
            if let Some(state) = self.role.creator_state_mut() {
                state.tried_params.insert(params.clone(), support);
                state.reoffer_attempts += 1;
            }
            (params, initial_state)
        } else if self.deadline.is_none() {
            (
                self.offer.data().params.as_bytes().to_vec(),
                self.offer.data().initial_state,
            )
        } else {
            return Err(self.timeout_error());
        };
        let new_deadline_ms = now_ms.saturating_add(REOFFER_WINDOW_MS);
        let new_deadline_ms = self.deadline.map_or(new_deadline_ms, |deadline| {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64;
            new_deadline_ms.min(now_ms.saturating_add(remaining))
        });
        let new_offer_data = OfferData::new(
            self.offer.data().negotiation_id,
            self.offer.data().offer_seq.saturating_add(1),
            self.offer.data().creator,
            self.offer.data().program_hash,
            self.offer.data().execution_profile,
            arena0_program::JsonBytes::try_new(params.clone())
                .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?,
            self.offer.data().target_size,
            initial_state,
            new_deadline_ms,
        )
        .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
        // Form the creator ticket from the immutable offer body first. The
        // returned offer therefore already contains its real creator hash;
        // no empty or placeholder ticket list can cross this boundary.
        let (offer, ticket) = self
            .book
            .create_creator_offer(new_offer_data, now_ms)
            .map_err(|_| NegotiationDriveError::InvalidLocalTicket)?;
        self.offer = offer;
        // The re-offer is a new slot. Retire lower offer sequences after the
        // valid slot is registered so stale tickets cannot consume capacity.
        self.book
            .register_offer(
                self.offer.data().negotiation_id,
                self.offer.data().offer_seq,
                &self.offer,
            )
            .map_err(|_| NegotiationDriveError::InvalidLocalTicket)?;
        self.book.prune_offers_before(
            self.offer.data().negotiation_id,
            self.offer.data().offer_seq,
        );
        // The creator made the new offer: apply its ticket to the book and
        // retain it for the local signing/fetch path before gossiping either
        // value.
        self.book
            .apply_ticket(&ticket)
            .map_err(|_| NegotiationDriveError::InvalidLocalTicket)?;
        self.local_ticket = Some(ticket.clone());
        if let Some(supervision) = &self.supervision {
            let _ = supervision.ticket.send(Some(ticket));
            let _ = supervision.offer.send(self.offer.clone());
        }
        if let Some(state) = self.role.creator_state_mut() {
            state.frozen = None;
            state.activation_signatures.clear();
        }
        self.emitted_signature = false;
        self.refresh_fetch_registration();
        Ok(())
    }

    // ── Participant: fetch, prepare, sign, commit ───────────────────────

    /// The participant's acceptance policy for a re-offer: the sequence is
    /// monotonic, the creator is unchanged, the params match the local
    /// preference (when one is set), and the offer's initial state matches
    /// the local computation for those params.
    fn accept_reoffer(&mut self, offer: &Offer) -> bool {
        if offer.data().negotiation_id != self.offer.data().negotiation_id
            || offer.data().creator != self.offer.data().creator
            || offer.data().execution_profile != self.offer.data().execution_profile
            || offer.data().offer_seq <= self.offer.data().offer_seq
        {
            return false;
        }
        if let Some(params) = &self.preferred_params
            && offer.data().params.as_bytes() != params.as_slice()
        {
            return false;
        }
        match (self.recompute_initial_state)(offer.data().params.as_bytes()) {
            Ok(initial_state) => initial_state == offer.data().initial_state,
            Err(_) => false,
        }
    }

    /// Switch to an authenticated re-offer: a fresh ticket for the new
    /// `offer_seq`, a fresh book (old tickets die by reference) seeded with
    /// the creator's authenticated ticket, and a reset of the prepared state.
    fn switch_offer(
        &mut self,
        offer: Offer,
        creator_ticket: Ticket,
    ) -> Result<(), NegotiationDriveError> {
        if self.role.participant_state().is_none() {
            return Err(NegotiationDriveError::InvalidLocalTicket);
        }
        offer
            .data()
            .validate()
            .map_err(|_| NegotiationDriveError::InvalidLocalTicket)?;
        self.offer = offer;
        if let Some(supervision) = &self.supervision {
            let _ = supervision.offer.send(self.offer.clone());
        }
        // The book accumulates offer slots: the switched-to offer is a new
        // slot seeded with the creator's authenticated ticket.
        self.book
            .register_offer(
                self.offer.data().negotiation_id,
                self.offer.data().offer_seq,
                &self.offer,
            )
            .map_err(|_| NegotiationDriveError::InvalidLocalTicket)?;
        self.book.prune_offers_before(
            self.offer.data().negotiation_id,
            self.offer.data().offer_seq,
        );
        self.book
            .apply_ticket(&creator_ticket)
            .map_err(|_| NegotiationDriveError::InvalidLocalTicket)?;
        self.issue_local_ticket()
            .map_err(|_| NegotiationDriveError::InvalidLocalTicket)?;
        if let Some(state) = self.role.participant_state_mut() {
            state.prepared = None;
            state.fetch_sent_at = None;
        }
        self.emitted_signature = false;
        self.refresh_fetch_registration();
        Ok(())
    }

    /// The convergence fetch: before producing its activation signature, the
    /// participant pulls any missed tickets from the creator — a bounded,
    /// one-shot pull, the only pull in the design. The request is resent when
    /// the response is late.
    async fn try_fetch_and_prepare(&mut self) -> Result<(), NegotiationDriveError> {
        if self
            .role
            .participant_state()
            .is_none_or(|state| state.prepared.is_some())
        {
            return Ok(());
        }
        // Defer until the local ticket is signed: the participant consents
        // only after the creator's Active ticket authenticates the offer (a
        // topic event), and the fetch response is consumed against that
        // ticket. Fetching before authentication would race the response.
        if self.local_ticket.is_none() {
            return Ok(());
        }
        if self.offer.tickets().len() != self.offer.data().target_size as usize {
            return Ok(());
        }
        let now = Instant::now();
        let should_fetch = self.role.participant_state().is_some_and(|state| {
            state
                .fetch_sent_at
                .is_none_or(|sent| now.duration_since(sent) > FETCH_TIMEOUT)
        });
        if should_fetch {
            self.send_fetch_request().await?;
            if let Some(state) = self.role.participant_state_mut() {
                state.fetch_sent_at = Some(now);
            }
        }
        Ok(())
    }

    async fn send_fetch_request(&mut self) -> Result<(), NegotiationDriveError> {
        let session_hash = self.expected_session_hash()?;
        let creator = self.offer.data().creator;
        let operation_deadline = self.operation_deadline();
        let Ok(Ok(send)) =
            timeout_at(operation_deadline, self.transport.open_fetch(&creator)).await
        else {
            return Ok(());
        };
        let request = FetchFrame::FetchActivationTickets(FetchActivationTickets { session_hash });
        let _ = timeout_at(operation_deadline, send.send_fetch(&request)).await;
        Ok(())
    }

    /// Consume the creator's fetch response: validate the authenticated
    /// remote, the exact ticket set, and the prepare window; durably prepare
    /// the `SessionHash`; then broadcast the activation signature. The
    /// response frame was already consumed by the accept router (which
    /// routed by its session hash).
    async fn consume_fetch(
        &mut self,
        recv: RecvHandle,
        response: ActivationTickets,
    ) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        if *recv.remote_peer() != self.offer.data().creator {
            return Ok(None);
        }
        if self
            .role
            .participant_state()
            .is_none_or(|state| state.prepared.is_some())
        {
            return Ok(None);
        }
        if response.session_hash != self.expected_session_hash()? {
            return Ok(None);
        }
        if response.tickets.len() != self.offer.tickets().len() {
            return Ok(None);
        }
        let offer_hash = OfferHash::of(self.offer.data());
        for (ticket, expected) in response.tickets.iter().zip(self.offer.tickets()) {
            // The exact hash is not enough: the outer signature is not part
            // of the hash, so every fetched ticket's identity signature and
            // scope-bound key_binding are verified too.
            if TicketHash::of(&ticket.data) != *expected
                || ticket.verify_for_offer(&offer_hash).is_err()
            {
                return Ok(None);
            }
            if !matches!(ticket.data.action, TicketAction::Active { .. }) {
                return Ok(None);
            }
        }
        if !self.prepare_window_ok(&response.tickets) {
            return Ok(None);
        }
        // The local ticket must be part of the frozen selection: an
        // unselected peer must not prepare, sign, or commit another
        // peer's session. An unsigned local ticket (the offer not yet
        // authenticated) makes the response inert, never a panic.
        let Some(local_ticket) = &self.local_ticket else {
            return Ok(None);
        };
        let local_hash = TicketHash::of(&local_ticket.data);
        if !self.offer.tickets().contains(&local_hash) {
            return Err(NegotiationDriveError::NotSelected);
        }
        let activation = match PreparedActivation::new(self.offer.clone(), response.tickets.clone())
        {
            Ok(activation) => activation,
            Err(error) => {
                return Err(NegotiationDriveError::Signing(error.to_string()));
            }
        };
        match (self.prepare)(&mut *self.execution_store, activation.clone())
            .await
            .map_err(NegotiationDriveError::Prepare)?
        {
            PrepareOutcome::Accepted => {}
            PrepareOutcome::NotPreparable(_) => return Ok(None),
            PrepareOutcome::Conflict => {
                return Err(NegotiationDriveError::Prepare(
                    "local execution is bound to different preparation facts".into(),
                ));
            }
        }
        self.arm_completion_deadline();
        let session_hash = activation.session_hash();
        if let Some(state) = self.role.participant_state_mut() {
            state.prepared = Some(activation);
            state.fetch_sent_at = None;
        }
        self.emit_negotiation_event(NegotiationEvent::ActivationPrepared {
            session_hash,
            participant_count: bounded_count(self.offer.data().target_size as usize),
        });
        self.publish_local_signature().await?;
        Ok(None)
    }

    /// Commit on the session announcement: verify the full certificate chain
    /// and durably persist the final activation.
    async fn on_announcement(
        &mut self,
        announcement: ActivationAnnouncement,
    ) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        let Some(prepared) = self
            .role
            .participant_state()
            .and_then(|state| state.prepared.clone())
        else {
            return Ok(None);
        };
        // An invalid announcement from gossip is inert untrusted input, not a
        // fatal error: the structural checks and the session-hash match gate
        // the commit.
        let Ok(activation) = Activation::from_announcement(prepared.clone(), announcement) else {
            return Ok(None);
        };
        // An invalid announcement from gossip is inert untrusted input: the
        // full chain (identity signatures, key bindings, aggregate) is
        // verified here, and a failure never aborts the drive.
        let Ok(activated) = ActivatedSession::new(activation) else {
            return Ok(None);
        };
        match (self.persist_activation)(&mut *self.execution_store, activated.activation().clone())
            .await
            .map_err(NegotiationDriveError::PersistCommit)?
        {
            DurableOutcome::Accepted => {}
            DurableOutcome::Conflict => {
                return Err(NegotiationDriveError::PersistCommit(
                    "local execution is bound to another activation".into(),
                ));
            }
        }
        self.emit_negotiation_event(NegotiationEvent::ActivationCommitted {
            session_hash: prepared.session_hash(),
            participant_count: bounded_count(self.offer.data().target_size as usize),
        });
        Ok(Some(activated))
    }

    // ── Fetch serving (creator) ─────────────────────────────────────────

    async fn on_fetch_stream(
        &mut self,
        recv: RecvHandle,
        frame: FetchFrame,
    ) -> Result<Option<ActivatedSession>, NegotiationDriveError> {
        if let Some(frozen) = self
            .role
            .creator_state()
            .and_then(|state| state.frozen.clone())
        {
            let FetchFrame::FetchActivationTickets(request) = frame else {
                return Ok(None);
            };
            serve_fetch_evidence(
                &self.transport,
                &recv,
                request,
                frozen.session_hash(),
                frozen.tickets(),
                self.operation_deadline(),
            )
            .await;
        } else if self.role.participant_state().is_some() {
            let FetchFrame::ActivationTickets(response) = frame else {
                return Ok(None);
            };
            return self.consume_fetch(recv, response).await;
        }
        Ok(None)
    }

    /// Publish the local activation signature over the prepared
    /// (participant) or frozen (creator) activation. Re-published after a
    /// resume: the pre-crash emission may not have reached the creator.
    async fn publish_local_signature(
        &mut self,
    ) -> Result<ActivationSignature, NegotiationDriveError> {
        let activation = self
            .role
            .prepared_activation()
            .expect("prepared activation evidence exists when publishing its signature")
            .clone();
        let signature = self.sign_bls(&activation.activation_data().signing_bytes())?;
        let local_hash = TicketHash::of(
            &self
                .local_ticket
                .as_ref()
                .expect("the local ticket is signed before the activation signature")
                .data,
        );
        let frame = ActivationSignature::new(activation.session_hash(), local_hash, signature)
            .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
        self.publish(NegotiationFact::ActivationSignature(frame.clone()))
            .await?;
        self.emitted_signature = true;
        Ok(frame)
    }

    // ── Withdrawals ─────────────────────────────────────────────────────

    fn withdrawal_requested(&self) -> bool {
        self.supervision
            .as_ref()
            .is_some_and(|supervision| *supervision.withdrawal_requested.borrow())
    }

    async fn on_withdrawal_requested(&mut self) -> Result<bool, NegotiationDriveError> {
        // A prepared activation is the point of no return. The daemon will
        // observe the resulting Activating/Active lifecycle and reject the
        // cancellation request rather than allowing a local flag to race the
        // durable activation boundary.
        if self.role.prepared_activation().is_some() {
            return Ok(false);
        }

        // A participant may be cancelled before the creator's offer is
        // authenticated and therefore before it has a local ticket. There is
        // no signed wire fact to publish in that state, but the local drive
        // must still stop and durably record the cancellation.
        let Some(current) = self.local_ticket.as_ref() else {
            return Ok(true);
        };
        if !matches!(&current.data.action, TicketAction::Active { .. }) {
            return Ok(true);
        }
        let revision =
            current.data.revision.checked_add(1).ok_or_else(|| {
                NegotiationDriveError::Signing("ticket revision exhausted".into())
            })?;
        let data = TicketData::new(
            current.data.negotiation_id,
            current.data.offer_seq,
            current.data.signer,
            revision,
            TicketAction::Withdrawn,
        )
        .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
        let ticket = Ticket {
            signature: self.identity.sign(&data.signing_bytes()),
            data,
        };
        self.publish(NegotiationFact::Ticket(ticket)).await?;
        Ok(true)
    }

    async fn on_local_withdrawal(
        &mut self,
        withdrawal: LocalTicketWithdrawal,
    ) -> Result<bool, NegotiationDriveError> {
        // Prepared consent is irrevocable: once the exact SessionHash is
        // durably prepared (the creator's frozen boundary or the
        // participant's prepared boundary), the local ticket cannot be
        // withdrawn.
        if self.role.prepared_activation().is_some() {
            let _ = withdrawal.accepted.send(false);
            return Ok(false);
        }
        self.publish(NegotiationFact::Ticket(withdrawal.ticket.clone()))
            .await?;
        let _ = withdrawal.accepted.send(true);
        Ok(true)
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    fn expected_session_hash(&self) -> Result<SessionHash, NegotiationDriveError> {
        let activation_data = ActivationData::new(
            OfferHash::of(self.offer.data()),
            self.offer.tickets().to_vec(),
        )
        .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
        Ok(SessionHash::of(&activation_data))
    }

    fn arm_completion_deadline(&mut self) {
        if self.deadline.is_none() {
            self.deadline = Some(Instant::now() + COMPLETION_TIMEOUT);
        }
    }

    fn operation_deadline(&self) -> Instant {
        self.deadline
            .unwrap_or_else(|| Instant::now() + FETCH_TIMEOUT)
    }

    /// Every selected Active ticket must pass the prepare-window check before
    /// a signer durably prepares and activation-signs.
    fn prepare_window_ok(&self, tickets: &[Ticket]) -> bool {
        let now = unix_time_ms();
        // The offer deadline gates admission: past it, no new prepare.
        if now >= self.offer.data().deadline_unix_ms {
            return false;
        }
        tickets
            .iter()
            .all(|ticket| ticket_prepare_window_ok(ticket, now))
    }

    fn sign_bls(&self, message: &[u8]) -> Result<BlsSignature, NegotiationDriveError> {
        Ok(self.execution.sign(message))
    }

    async fn publish(&mut self, fact: NegotiationFact) -> Result<(), NegotiationDriveError> {
        let frame = NegotiationGossip::new(
            self.offer.data().program_hash,
            self.offer.data().negotiation_id,
            fact,
        );
        frame
            .validate()
            .map_err(|error| NegotiationDriveError::Signing(error.to_string()))?;
        match timeout_at(
            self.operation_deadline(),
            self.topic.publish(frame.signing_bytes().into()),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(NegotiationDriveError::StreamClosed),
            Err(_) => Err(self.timeout_error()),
        }
    }

    async fn rejoin_topic(&mut self) -> Result<(), NegotiationDriveError> {
        let bootstrap = self.bootstrap_peers();
        let _ = self.topic.close().await;
        let topic = match timeout_at(
            self.operation_deadline(),
            self.transport
                .subscribe_program(self.offer.data().program_hash, bootstrap),
        )
        .await
        {
            Ok(Ok(topic)) => topic,
            Ok(Err(_)) => return Err(self.stream_failure()),
            Err(_) => return Err(self.timeout_error()),
        };
        self.topic = topic;
        self.neighbors.clear();
        self.emit_negotiation_event(NegotiationEvent::TopicRejoined);
        Ok(())
    }

    fn stream_failure(&self) -> NegotiationDriveError {
        if self
            .role
            .participant_state()
            .is_some_and(|state| state.prepared.is_some())
            || self.emitted_signature
        {
            NegotiationDriveError::UnknownOutcome
        } else {
            NegotiationDriveError::StreamClosed
        }
    }

    fn bootstrap_peers(&self) -> Vec<PeerId> {
        let mut peers = self
            .book
            .ticket_set(
                self.offer.data().negotiation_id,
                self.offer.data().offer_seq,
            )
            .into_iter()
            .map(|ticket| ticket.data.signer)
            .chain(std::iter::once(self.offer.data().creator))
            .filter(|peer| *peer != self.identity.peer_id())
            .collect::<Vec<_>>();
        peers.sort_unstable();
        peers.dedup();
        peers
    }

    fn next_cadence(&self) -> Instant {
        next_cadence(self.identity.peer_id(), self.cadence_counter)
    }

    fn timeout_error(&mut self) -> NegotiationDriveError {
        if self
            .role
            .participant_state()
            .is_some_and(|state| state.prepared.is_some())
            || self.emitted_signature
        {
            NegotiationDriveError::UnknownOutcome
        } else {
            if !self.timed_out_emitted {
                self.timed_out_emitted = true;
                self.emit_negotiation_event(NegotiationEvent::TimedOut {
                    stage: self.negotiation_stage(),
                    ticket_count: bounded_count(self.book.len(
                        self.offer.data().negotiation_id,
                        self.offer.data().offer_seq,
                    )),
                    sig_count: bounded_count(
                        self.role
                            .creator_state()
                            .map_or(0, |state| state.activation_signatures.len()),
                    ),
                    target_size: self.offer.data().target_size,
                });
            }
            NegotiationDriveError::Timeout
        }
    }

    fn emit_negotiation_event(&self, event: NegotiationEvent) {
        (self.event_sink)(
            EventSource::Negotiation {
                peer_id: self.identity.peer_id(),
                exec_id: self.exec_id,
                program_id: self.offer.data().program_hash,
                negotiation_id: self.offer.data().negotiation_id,
            },
            event,
        );
    }

    fn emit_ticket_accepted(&self, ticket: &Ticket) {
        self.emit_negotiation_event(NegotiationEvent::TicketAccepted {
            participant: ticket.data.signer,
            ticket_hash: TicketHash::of(&ticket.data),
            ticket_count: bounded_count(self.book.len(
                self.offer.data().negotiation_id,
                self.offer.data().offer_seq,
            )),
            target_size: self.offer.data().target_size,
        });
    }

    fn emit_retry(&self) {
        self.emit_negotiation_event(NegotiationEvent::Retry {
            attempt: self.cadence_counter,
            stage: self.negotiation_stage(),
            ticket_count: bounded_count(self.book.len(
                self.offer.data().negotiation_id,
                self.offer.data().offer_seq,
            )),
            sig_count: bounded_count(
                self.role
                    .creator_state()
                    .map_or(0, |state| state.activation_signatures.len()),
            ),
            target_size: self.offer.data().target_size,
        });
    }

    fn negotiation_stage(&self) -> NegotiationStage {
        if self.role.prepared_activation().is_some() {
            NegotiationStage::Prepared
        } else {
            NegotiationStage::Gossiping
        }
    }
}

/// The next cadence deadline: 2 s ± 50 % jitter, deterministic per peer and
/// counter (no shared randomness needed).
fn next_cadence(peer_id: PeerId, counter: u64) -> Instant {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arena0/negotiation-cadence/v1");
    hasher.update(&peer_id.0);
    hasher.update(&counter.to_le_bytes());
    let digest = hasher.finalize();
    let jitter = u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("8 bytes")) % 1_000;
    let delay_ms = CADENCE_MS / 2 + CADENCE_MS * jitter / 1_000;
    Instant::now() + Duration::from_millis(delay_ms)
}

async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

fn bounded_count(count: usize) -> u16 {
    u16::try_from(count).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use arena0_crypto::{ExecutionKey, ExecutionSalt, NodeKeys, SecretKey};
    use arena0_program::ProgramHash;
    use arena0_protocol::{
        EventSource, ExecId, ExecutionAdmission, NegotiationEvent, NegotiationId, OfferData,
        PeerIdSource, StateHash,
    };
    use arena0_store::{ExecutionStore, Store, StoreConfig};
    use arena0_transport::{LocalNetwork, LocalTransport, Transport};
    use tokio::time::Instant;

    use super::super::NegotiationBook;
    use super::super::support::{
        DurableOutcome, NegotiationAttempt, NegotiationDriveError, NegotiationEffects,
        NegotiationStart, PersistActivationEffect, PrepareEffect, PrepareOutcome,
        RecomputeInitialStateEffect, unix_time_ms,
    };
    use super::NegotiationDriver;
    use crate::router::FetchRegistry;

    async fn test_driver<'a>(
        crypto: &'a NodeKeys,
        exec_id: ExecId,
        negotiation_id: NegotiationId,
        emit: &'a (dyn Fn(EventSource, NegotiationEvent) + Send + Sync),
    ) -> NegotiationDriver<'a, 'a> {
        let remote = NodeKeys::from_secret(SecretKey::from_bytes([2; 32]));
        let network = LocalNetwork::new();
        let [transport, _]: [LocalTransport; 2] =
            LocalTransport::create_network(&network, vec![crypto.peer_id(), remote.peer_id()])
                .expect("attach local transports")
                .try_into()
                .expect("two transports");
        let transport = Arc::new(transport);
        let wasm = vec![9; 4];
        let program_hash = ProgramHash::of(&wasm);
        let params = br#"{}"#.to_vec();
        let directory = Box::leak(Box::new(tempfile::tempdir().expect("temporary store")));
        let store = Box::leak(Box::new(
            Store::open(StoreConfig::new(
                directory.path().join("execution.sqlite"),
                crypto.peer_id(),
            ))
            .expect("store"),
        ));
        store
            .handle()
            .register_program(wasm, 1)
            .await
            .expect("program");
        let execution_store = Box::leak(Box::new(
            store
                .handle()
                .claim_execution(exec_id)
                .expect("execution writer"),
        ));
        execution_store
            .create_execution_request(
                program_hash,
                Some(arena0_program::JsonBytes::try_new(params.clone()).expect("params")),
                ExecutionAdmission::explicit(
                    negotiation_id,
                    vec![crypto.peer_id(), remote.peer_id()],
                )
                .expect("admission"),
                1,
            )
            .await
            .expect("request");
        let execution = Box::leak(Box::new(
            ExecutionKey::derive(
                &ExecutionSalt::try_from_bytes([7; 32]).expect("non-zero test salt"),
                &exec_id.0,
                &negotiation_id.0,
            )
            .expect("execution key"),
        ));
        let offer_data = OfferData::new(
            negotiation_id,
            0,
            crypto.peer_id(),
            program_hash,
            arena0_program::ExecutionProfile::current().hash(),
            arena0_program::JsonBytes::try_new(params.clone()).expect("valid JSON"),
            2,
            StateHash(*blake3::hash(&params).as_bytes()),
            unix_time_ms().saturating_add(30_000),
        )
        .expect("valid offer");
        let offer_builder = NegotiationBook::new(crypto, execution);
        let (offer, creator_ticket) = offer_builder
            .create_creator_offer(offer_data, unix_time_ms())
            .expect("creator offer");
        let topic = transport
            .subscribe_program(program_hash, Vec::new())
            .await
            .expect("topic subscription");
        let fetch_registry: FetchRegistry = Arc::new(Mutex::new(HashMap::new()));
        let prepare: PrepareEffect = Box::new(|store: &mut ExecutionStore, _| {
            Box::pin(async move {
                store
                    .load_execution_request()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok::<PrepareOutcome, String>(PrepareOutcome::Accepted)
            })
        });
        let persist: PersistActivationEffect = Box::new(|store: &mut ExecutionStore, _| {
            Box::pin(async move {
                store
                    .load_execution_request()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok::<DurableOutcome, String>(DurableOutcome::Accepted)
            })
        });
        let recompute: RecomputeInitialStateEffect = Box::new(|input: &[u8]| {
            Ok::<StateHash, String>(StateHash(*blake3::hash(input).as_bytes()))
        });
        let attempt = NegotiationAttempt {
            topic,
            exec_id,
            start: NegotiationStart::Fresh {
                offer,
                creator_ticket: Some(creator_ticket),
                preferred_params: None,
            },
            supervision: None,
            deadline: Some(Instant::now() + Duration::from_secs(30)),
        };
        let effects = NegotiationEffects {
            prepare,
            persist_activation: persist,
            recompute_initial_state: recompute,
            emit,
        };
        NegotiationDriver::new(
            transport,
            fetch_registry,
            crypto,
            execution,
            execution_store,
            attempt,
            effects,
        )
        .expect("driver construction")
    }

    fn events_for(
        events: &Mutex<Vec<(EventSource, NegotiationEvent)>>,
        exec_id: ExecId,
    ) -> Vec<NegotiationEvent> {
        events
            .lock()
            .expect("event lock")
            .iter()
            .filter(|(source, _)| {
                matches!(
                    source,
                    EventSource::Negotiation { exec_id: event_exec_id, .. }
                        if *event_exec_id == exec_id
                )
            })
            .map(|(_, event)| event.clone())
            .collect()
    }

    #[tokio::test]
    async fn open_creator_renews_beyond_tuning_and_book_limits() {
        let crypto = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
        let emit = |_, _| {};
        let negotiation_id = NegotiationId([8; 32]);
        let mut driver = test_driver(&crypto, ExecId([7; 32]), negotiation_id, &emit).await;
        driver.deadline = None;
        let params = driver.offer.data().params.clone();
        let initial_state = driver.offer.data().initial_state;
        for seq in 1..=100 {
            driver.try_reoffer().expect("same terms remain open");
            assert_eq!(driver.offer.data().negotiation_id, negotiation_id);
            assert_eq!(driver.offer.data().offer_seq, seq);
            assert_eq!(driver.offer.data().params, params);
            assert_eq!(driver.offer.data().initial_state, initial_state);
            assert_eq!(driver.book.len(negotiation_id, seq - 1), 0);
            assert_eq!(driver.book.len(negotiation_id, seq), 1);
        }
        assert!(driver.deadline.is_none());
        assert_eq!(driver.role.creator_state().unwrap().reoffer_attempts, 0);
    }

    #[tokio::test]
    async fn oversubscribed_creator_freezes_exactly_the_requested_count() {
        let crypto = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
        let emit = |_, _| {};
        let negotiation_id = NegotiationId([8; 32]);
        let mut driver = test_driver(&crypto, ExecId([7; 32]), negotiation_id, &emit).await;
        driver.deadline = None;
        let creator_ticket = driver.local_ticket.clone().unwrap();
        let mut peers = Vec::new();
        for seed in [2, 3] {
            let peer = NodeKeys::from_secret(SecretKey::from_bytes([seed; 32]));
            peers.push(peer.peer_id());
            let execution = ExecutionKey::derive(
                &ExecutionSalt::try_from_bytes([seed; 32]).unwrap(),
                &[seed; 32],
                &negotiation_id.0,
            )
            .unwrap();
            let mut book = NegotiationBook::new(&peer, &execution);
            book.register_offer(negotiation_id, 0, &driver.offer)
                .unwrap();
            book.apply_ticket(&creator_ticket).unwrap();
            let ticket = book
                .issue_ticket(negotiation_id, 0, 0, unix_time_ms())
                .unwrap();
            driver.book.apply_ticket(&ticket).unwrap();
        }
        assert_eq!(driver.book.len(negotiation_id, 0), 3);
        driver.try_freeze().await.expect("freeze excess tickets");
        let frozen = driver
            .role
            .creator_state()
            .unwrap()
            .frozen
            .as_ref()
            .unwrap();
        assert_eq!(frozen.tickets().len(), 2);
        assert_eq!(frozen.tickets()[0].data.signer, crypto.peer_id());
        assert_eq!(
            frozen.tickets()[1].data.signer,
            *peers.iter().min().unwrap()
        );
        assert!(driver.deadline.is_some());
    }

    #[tokio::test]
    async fn unsigned_complete_offer_cannot_reject_or_time_out_a_waiting_join() {
        let crypto = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
        let emit = |_, _| {};
        let negotiation_id = NegotiationId([8; 32]);
        let mut driver = test_driver(&crypto, ExecId([7; 32]), negotiation_id, &emit).await;
        driver.deadline = None;
        let remote = NodeKeys::from_secret(SecretKey::from_bytes([2; 32]));
        let mut data = driver.offer.data().clone();
        data.creator = remote.peer_id();
        let creator_book = NegotiationBook::new(&remote, driver.execution);
        let (offer, ticket) = creator_book
            .create_creator_offer(data, unix_time_ms())
            .unwrap();
        driver.offer = offer;
        driver.role = super::NegotiationRole::Participant(super::ParticipantState::default());
        driver.book = NegotiationBook::new(&crypto, driver.execution);
        driver
            .book
            .register_offer(negotiation_id, 0, &driver.offer)
            .unwrap();
        driver.book.apply_ticket(&ticket).unwrap();
        driver.issue_local_ticket().unwrap();
        // Only the offer body is authenticated. This fabricated complete
        // hash vector excludes local consent and has no creator response.
        driver.offer = arena0_protocol::Offer::new(
            driver.offer.data().clone(),
            vec![
                arena0_protocol::TicketHash::of(&ticket.data),
                arena0_protocol::TicketHash([9; 32]),
            ],
        )
        .unwrap();
        driver
            .try_fetch_and_prepare()
            .await
            .expect("untrusted selection stays pending");
        assert!(driver.deadline.is_none());
        assert!(driver.role.participant_state().unwrap().prepared.is_none());
    }

    #[tokio::test]
    async fn rejoin_emits_one_topic_rejoined_fact_after_replacement() {
        let crypto = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
        let exec_id = ExecId([7; 32]);
        let events = Mutex::new(Vec::new());
        let emit = |source, event| events.lock().expect("event lock").push((source, event));
        let mut driver = test_driver(&crypto, exec_id, NegotiationId([8; 32]), &emit).await;

        driver.rejoin_topic().await.expect("topic replacement");

        let facts = events_for(&events, exec_id)
            .into_iter()
            .filter(|event| matches!(event, NegotiationEvent::TopicRejoined))
            .count();
        assert_eq!(facts, 1);
    }

    #[tokio::test]
    async fn timeout_emits_one_fact_even_when_reported_twice() {
        let crypto = NodeKeys::from_secret(SecretKey::from_bytes([3; 32]));
        let exec_id = ExecId([9; 32]);
        let events = Mutex::new(Vec::new());
        let emit = |source, event| events.lock().expect("event lock").push((source, event));
        let mut driver = test_driver(&crypto, exec_id, NegotiationId([10; 32]), &emit).await;

        assert!(matches!(
            driver.timeout_error(),
            NegotiationDriveError::Timeout
        ));
        assert!(matches!(
            driver.timeout_error(),
            NegotiationDriveError::Timeout
        ));

        let facts = events_for(&events, exec_id)
            .into_iter()
            .filter(|event| matches!(event, NegotiationEvent::TimedOut { .. }))
            .collect::<Vec<_>>();
        assert_eq!(facts.len(), 1);
        assert!(matches!(
            &facts[0],
            NegotiationEvent::TimedOut {
                ticket_count: 1,
                sig_count: 0,
                target_size: 2,
                ..
            }
        ));
    }
}
