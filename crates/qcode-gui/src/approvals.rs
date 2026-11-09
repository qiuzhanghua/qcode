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

    /// Take every still-pending prompt, in arrival order.
    ///
    /// Used when a background session's state is merged into the view the user
    /// just switched to: an approval blocks the engine until someone answers
    /// it, so it must survive the trip (ADR-0058 §3).
    pub fn drain(&mut self) -> Vec<ApprovalView> {
        self.pending.drain(..).collect()
    }
}
