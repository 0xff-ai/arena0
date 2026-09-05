use super::{ExecId, HostName, PendingId, TextArea, Value, oneshot};
use std::collections::VecDeque;

#[derive(Debug)]
pub(super) struct PendingCallout {
    pub(super) host: HostName,
    pub(super) exec_id: ExecId,
    pub(super) pending_id: PendingId,
    pub(super) callout_index: u32,
    pub(super) name: String,
    pub(super) prompt: String,
    pub(super) context: Value,
    pub(super) schema: Value,
    pub(super) editor: TextArea<'static>,
    pub(super) scroll: u16,
    pub(super) validation_error: Option<String>,
    pub(super) reply: oneshot::Sender<Value>,
}

impl PendingCallout {
    fn same_request(
        &self,
        host: &HostName,
        exec_id: ExecId,
        pending_id: PendingId,
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

    pub(super) fn next(&mut self) {
        if !self.is_empty() {
            self.selected = (self.selected + 1) % self.len();
        }
    }
    pub(super) fn previous(&mut self) {
        if !self.is_empty() {
            self.selected = (self.selected + self.len() - 1) % self.len();
        }
    }
    pub(super) fn remove_selected(&mut self) -> Option<PendingCallout> {
        let request = self.requests.remove(self.selected);
        self.selected = 0;
        request
    }
    pub(super) fn clear(&mut self) {
        self.requests.clear();
        self.selected = 0;
    }
}
