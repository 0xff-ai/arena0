use super::{CalloutId, ExecId, HostName, TextArea, Value, oneshot};
use std::collections::VecDeque;

#[derive(Debug)]
pub(super) struct PendingCallout {
    pub(super) host: HostName,
    pub(super) exec_id: ExecId,
    pub(super) pending_id: CalloutId,
    pub(super) callout_index: u32,
    pub(super) name: String,
    pub(super) prompt: String,
    pub(super) context: Value,
    pub(super) schema: Value,
    pub(super) editor: TextArea<'static>,
    pub(super) scroll: u16,
    pub(super) validation_error: Option<String>,
    /// The monitor or run driver has sent this answer and is waiting for the
    /// daemon result. Keeping the editor protects the draft after a program
    /// rejection or an unknown transport outcome.
    pub(super) submitting: bool,
    pub(super) submission_error: Option<String>,
    pub(super) not_pending: bool,
    pub(super) reply: Option<oneshot::Sender<Value>>,
}

impl PendingCallout {
    fn same_request(
        &self,
        host: &HostName,
        exec_id: ExecId,
        pending_id: CalloutId,
        callout_index: u32,
    ) -> bool {
        self.host == *host
            && self.exec_id == exec_id
            && self.pending_id == pending_id
            && self.callout_index == callout_index
    }
}

/// Arrival order owns queue order; selection never moves a request or its draft.
#[derive(Debug, Default)]
pub(super) struct CalloutQueue {
    requests: VecDeque<PendingCallout>,
    selected: usize,
}

impl CalloutQueue {
    pub(super) fn push(&mut self, request: PendingCallout) -> bool {
        if self.requests.iter().any(|pending| {
            pending.same_request(
                &request.host,
                request.exec_id,
                request.pending_id,
                request.callout_index,
            )
        }) {
            return false;
        }
        self.requests.push_back(request);
        true
    }

    pub(super) fn len(&self) -> usize {
        self.requests.len()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }
    pub(super) fn selected(&self) -> Option<&PendingCallout> {
        self.requests.get(self.selected)
    }
    pub(super) fn selected_mut(&mut self) -> Option<&mut PendingCallout> {
        self.requests.get_mut(self.selected)
    }
    pub(super) fn position(&self) -> usize {
        if self.is_empty() {
            0
        } else {
            self.selected + 1
        }
    }
    pub(super) fn for_host(&self, host: &HostName) -> bool {
        self.requests.iter().any(|request| &request.host == host)
    }

    pub(super) fn name_for(
        &self,
        host: &HostName,
        exec_id: ExecId,
        pending_id: CalloutId,
    ) -> Option<&str> {
        self.requests
            .iter()
            .find(|request| {
                request.host == *host
                    && request.exec_id == exec_id
                    && request.pending_id == pending_id
            })
            .map(|request| request.name.as_str())
    }

    pub(super) fn select_for(&mut self, host: &HostName, exec_id: ExecId) -> bool {
        let Some(index) = self
            .requests
            .iter()
            .position(|request| {
                request.host == *host && request.exec_id == exec_id && !request.not_pending
            })
            .or_else(|| {
                self.requests
                    .iter()
                    .position(|request| request.host == *host && request.exec_id == exec_id)
            })
        else {
            return false;
        };
        self.selected = index;
        true
    }

    pub(super) fn select_for_pending(
        &mut self,
        host: &HostName,
        exec_id: ExecId,
        pending_id: CalloutId,
    ) -> bool {
        let Some(index) = self.requests.iter().position(|request| {
            request.host == *host
                && request.exec_id == exec_id
                && request.pending_id == pending_id
                && !request.not_pending
        }) else {
            return false;
        };
        self.selected = index;
        true
    }

    pub(super) fn get_mut(
        &mut self,
        host: &HostName,
        exec_id: ExecId,
        pending_id: CalloutId,
    ) -> Option<&mut PendingCallout> {
        self.requests.iter_mut().find(|request| {
            request.host == *host && request.exec_id == exec_id && request.pending_id == pending_id
        })
    }

    pub(super) fn next(&mut self) {
        if !self.is_empty() {
            self.selected = (self.selected + 1) % self.len();
        }
    }

    /// After sending an answer, present the next callout that can accept one.
    /// The submitted callout stays queued until the daemon responds.
    pub(super) fn select_next_answerable(&mut self) {
        for offset in 1..=self.len() {
            let index = (self.selected + offset) % self.len();
            if self.requests.get(index).is_some_and(|request| {
                !request.submitting && !request.not_pending && request.reply.is_some()
            }) {
                self.selected = index;
                break;
            }
        }
    }
    pub(super) fn previous(&mut self) {
        if !self.is_empty() {
            self.selected = (self.selected + self.len() - 1) % self.len();
        }
    }
    pub(super) fn clear(&mut self) {
        self.requests.clear();
        self.selected = 0;
    }

    pub(super) fn remove(
        &mut self,
        host: &HostName,
        exec_id: ExecId,
        pending_id: CalloutId,
    ) -> Option<PendingCallout> {
        let index = self.requests.iter().position(|request| {
            request.host == *host && request.exec_id == exec_id && request.pending_id == pending_id
        })?;
        let request = self.requests.remove(index);
        self.selected = self.selected.min(self.requests.len().saturating_sub(1));
        request
    }
}
