//! FIFO of pending question prompts for the GUI, mirroring `ApprovalQueue`.
//!
//! The engine may have several `ask_user` calls blocked at once; the window
//! shows one panel at a time and advances only after the user answers or the
//! turn is cancelled. A timed-out question is *not* removed here (the bridge
//! drops its pending entry silently), so the panel can flip to "continued" and
//! still accept a late answer.

use std::collections::VecDeque;

use gpui_kit::component::date_picker::DatePickerState;
use gpui_kit::component::input::InputState;
use gpui_kit::component::time_field::TimeFieldState;
use qcode_core::question::{AnswerItem, QuestionItem};
use qcode_ui_bridge::QuestionView;

use crate::shell::schedule::ScheduleRuleKind;

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

/// 「周期」题的结构化选择器状态：类型下拉 + 各自参数，与定时任务编辑表单的
/// 周期区同构（见 `shell/schedule.rs` 的 `ScheduleFormState`）。实体在 render
/// 里按 `(call_id, index)` 懒创建，面板换题/换批时重建；提交时由
/// [`Self::rule_text`] 拼成规范表达式回填给工具。
#[derive(Debug)]
pub struct RulePickerState {
    pub kind: ScheduleRuleKind,
    pub time: gpui_kit::Entity<TimeFieldState>,
    pub weekdays: Vec<u8>,
    pub monthly_day: gpui_kit::Entity<InputState>,
    pub interval_secs: gpui_kit::Entity<InputState>,
    pub once_picker: gpui_kit::Entity<DatePickerState>,
    pub cron_expr: gpui_kit::Entity<InputState>,
    pub rule_menu_open: bool,
    pub call_id: String,
    pub index: usize,
}

impl RulePickerState {
    /// 把选择器当前值拼成规范表达式（如 `daily 09:30`）；参数没填齐时返回
    /// `None`（调用方据此拒绝提交）。
    pub(crate) fn rule_text(&self, cx: &gpui_kit::App) -> Option<String> {
        match self.kind {
            ScheduleRuleKind::Daily | ScheduleRuleKind::Weekly | ScheduleRuleKind::Monthly => {
                let clock = self.time.read(cx).time().format("%H:%M");
                match self.kind {
                    ScheduleRuleKind::Daily => Some(format!("daily {clock}")),
                    ScheduleRuleKind::Weekly => {
                        let mut weekdays = self.weekdays.clone();
                        weekdays.sort_unstable();
                        weekdays.dedup();
                        if weekdays.is_empty() {
                            return None;
                        }
                        Some(format!(
                            "weekly {} {clock}",
                            weekdays
                                .iter()
                                .map(|day| day.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        ))
                    }
                    _ => {
                        let day = self.monthly_day.read(cx).value().trim().to_string();
                        if day.is_empty() {
                            return None;
                        }
                        Some(format!("monthly {day} {clock}"))
                    }
                }
            }
            ScheduleRuleKind::Interval => {
                let secs = self.interval_secs.read(cx).value().trim().to_string();
                if secs.is_empty() {
                    return None;
                }
                Some(format!("every {secs}"))
            }
            ScheduleRuleKind::Once => {
                let naive = self.once_picker.read(cx).date_time().start()?;
                Some(format!("once {}", naive.format("%Y-%m-%d %H:%M")))
            }
            ScheduleRuleKind::Cron => {
                let expr = self.cron_expr.read(cx).value().trim().to_string();
                if expr.is_empty() {
                    return None;
                }
                Some(format!("cron {expr}"))
            }
        }
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
    /// 「周期」题的选择器状态（`schedule_rule` 标记的那一题）。
    pub rule: Option<RulePickerState>,
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
            rule: None,
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

/// The free-text input of the question on screen, plus the subscription that
/// writes typed text into the panel's `customs`. Recreated whenever the panel
/// moves to a different question or batch.
#[derive(Debug)]
pub struct QuestionInputState {
    pub input: gpui_kit::Entity<gpui_kit::component::input::TextareaState>,
    pub call_id: String,
    pub index: usize,
    pub _subscription: gpui_kit::Subscription,
}
