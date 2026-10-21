//! FIFO of pending approvals for the GUI.
//!
//! The engine may have several tool calls blocked at once; the window shows one
//! prompt at a time and advances only after the user answers or the turn is
//! cancelled.

use std::collections::VecDeque;

use qcode_ui_bridge::ApprovalView;

#[derive(Debug, Default)]
pub struct ApprovalQueue {
    pending: VecDeque<ApprovalView>,
}

impl ApprovalQueue {
    pub fn push(&mut self, view: ApprovalView) {
        if !self
            .pending
            .iter()
            .any(|known| known.call_id == view.call_id)
        {
            self.pending.push_back(view);
        }
    }

    pub fn current(&self) -> Option<&ApprovalView> {
        self.pending.front()
    }

    pub fn take_current(&mut self) -> Option<ApprovalView> {
        self.pending.pop_front()
    }

    pub fn closed(&mut self, call_id: &str) {
        self.pending.retain(|view| view.call_id != call_id);
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}
