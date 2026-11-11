//! FIFO of pending question prompts for the GUI, mirroring `ApprovalQueue`.
//!
//! The engine may have several `ask_user` calls blocked at once; the window
//! shows one panel at a time and advances only after the user answers or the
//! turn is cancelled. A timed-out question is *not* removed here (the bridge
//! drops its pending entry silently), so the panel can flip to "continued" and
//! still accept a late answer.

use std::collections::VecDeque;

use qcode_core::question::{AnswerItem, QuestionItem};
use qcode_ui_bridge::QuestionView;

#[derive(Debug, Default)]
pub struct QuestionQueue {
    pending: VecDeque<QuestionView>,
}

impl QuestionQueue {
    pub fn push(&mut self, view: QuestionView) {
        if !self
            .pending
            .iter()
            .any(|known| known.call_id == view.call_id)
        {
            self.pending.push_back(view);
        }
    }

    pub fn current(&self) -> Option<&QuestionView> {
        self.pending.front()
    }

    pub fn take_current(&mut self) -> Option<QuestionView> {
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

    /// Take every still-pending prompt, in arrival order (used when a
    /// background session is merged into the view).
    pub fn drain(&mut self) -> Vec<QuestionView> {
        self.pending.drain(..).collect()
    }
}

/// The panel's interactive state for the question currently on screen.
#[derive(Debug)]
pub struct QuestionPanel {
    pub view: QuestionView,
    /// The question currently shown (pager).
    pub index: usize,
    /// Selected option indices per question.
    pub selected: Vec<Vec<usize>>,
    /// Free-text custom answers per question.
    pub customs: Vec<String>,
    /// The timed window elapsed: answers go through the steering path.
    pub continued: bool,
    /// A submit is in flight; the panel is frozen.
    pub submitting: bool,
    /// Host-side deadline for a timed question (`Instant`), for the countdown.
    pub deadline: Option<std::time::Instant>,
}

impl QuestionPanel {
    pub fn new(view: QuestionView) -> Self {
        let count = view.questions.len();
        let deadline = view
            .timeout_secs
            .filter(|seconds| *seconds > 0)
            .map(|seconds| {
                std::time::Instant::now() + std::time::Duration::from_secs(seconds as u64)
            });
        Self {
            view,
            index: 0,
            selected: vec![Vec::new(); count],
            customs: vec![String::new(); count],
            continued: false,
            submitting: false,
            deadline,
        }
    }

    pub fn current(&self) -> &QuestionItem {
        &self.view.questions[self.index]
    }

    pub fn is_single(&self) -> bool {
        !self.current().multi_select
    }

    /// Toggle one option of the current question.
    pub fn toggle(&mut self, option: usize) {
        let multi = self.current().multi_select;
        let slot = &mut self.selected[self.index];
        if let Some(position) = slot.iter().position(|index| *index == option) {
            slot.remove(position);
        } else if multi {
            slot.push(option);
        } else {
            slot.clear();
            slot.push(option);
        }
    }

    pub fn is_selected(&self, option: usize) -> bool {
        self.selected[self.index].contains(&option)
    }

    pub fn set_custom(&mut self, text: String) {
        self.customs[self.index] = text;
    }

    /// Selected labels of the current question, as the model sees them.
    pub fn selected_labels(&self) -> Vec<String> {
        let question = self.current();
        self.selected[self.index]
            .iter()
            .map(|index| question.options[*index].label.clone())
            .collect()
    }

    /// Move to the next question. `false` when the batch is complete.
    pub fn advance(&mut self) -> bool {
        if self.index + 1 >= self.view.questions.len() {
            return false;
        }
        self.index += 1;
        true
    }

    pub fn back(&mut self) -> bool {
        if self.index == 0 {
            return false;
        }
        self.index -= 1;
        true
    }

    /// Skip the current question (empty answer), then advance.
    pub fn skip(&mut self) -> bool {
        self.selected[self.index].clear();
        self.customs[self.index] = String::new();
        self.advance()
    }

    /// Build the answer batch covering every question exactly once.
    pub fn answers(&self) -> Vec<AnswerItem> {
        self.view
            .questions
            .iter()
            .enumerate()
            .map(|(i, question)| {
                let custom = self.customs[i].trim();
                AnswerItem {
                    id: question.id.clone(),
                    selected: self.selected[i]
                        .iter()
                        .map(|index| question.options[*index].label.clone())
                        .collect(),
                    custom: if custom.is_empty() {
                        None
                    } else {
                        Some(custom.to_string())
                    },
                }
            })
            .collect()
    }
}
