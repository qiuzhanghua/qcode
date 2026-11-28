//! The 技能 list, its read-only content dialog, and the per-skill switches
//! (ADR-0110).
//!
//! Three things live here rather than in `render.rs`, because all three are//! about *state*, not layout: which index the rows come from (§2), what a
//! switch writes (§4/§5), and which skill the dialog has open (§7).
//!
//! The list is deliberately not a `forms::TABLES` table: that path is a paged
//! grid of input cells, and this page wants "scroll to the bottom" over
//! read-only rows.

use std::sync::Arc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::text::TextView;
// `*` re-exports `gpui_macros::test`; see the warning on `setup::mod`'s imports.
use gpui_kit::{div, prelude::*, *};
use qcode_kernel::config::Config;
use qcode_tools::skills::{Skill, SkillIndex, SkillSummary};
use std::path::PathBuf;

use super::{AutoSaveKind, SetupView};

/// The skill whose content dialog is open.
///
/// Not `SkillView`: that name is already `qcode-ui-state`'s read-only metadata
/// for the `/skills` command, and two types with one name in one GUI is how a
/// later reader picks the wrong one.
///
/// The `Skill` is a snapshot, so the 64KiB body is read once on open rather
/// than cloned per frame. A TTL refresh while the dialog is open therefore
/// leaves the dialog showing what was opened — which is what a read-only panel
/// should do.
pub(crate) struct OpenSkill {
    pub(crate) key: String,
    pub(crate) skill: Skill,
    /// When 复制 was last pressed, for the 「已复制」 flash (1.5s, same as the
    /// transcript's copy buttons).
    pub(crate) copied_at: Option<std::time::Instant>,
}

// A `[skills] disabled` entry that matches nothing discovered is left visible
// instead of dropped: a typo, a renamed server or a deleted directory all look
// identical from here, and silently ignoring them means the config accumulates
// entries that do nothing.

/// What a switch writes: the config's off-list with `key` on or off.
///
/// Pulled out of the view because it is the whole of ADR-0110 §5's write rule and
/// it needs no `Context` to be right — which is what makes it testable at all (the
/// view methods below need a window, and this crate's headless harness cannot be
/// built on this machine; see the ADR's 测试 section).
///
/// Entries are compared by their *parsed* form, so `pdf` and ` pdf ` are the same
/// skill and switching one off does not leave the other behind as a duplicate.
fn toggle_disabled(raw: &[String], key: &str, enabled: bool) -> Vec<String> {
    let mut keys: Vec<String> = raw
        .iter()
        .filter(|entry| {
            qcode_tools::skills::DisableKey::parse(entry)
                .map(|parsed| parsed.render() != key)
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if !enabled && !keys.iter().any(|entry| entry == key) {
        keys.push(key.to_string());
    }
    keys
}

/// The off-list entries that match no row (ADR-0110 §6).
///
/// A bare name matches a row **by name**, because that is what a bare name means:
/// every origin answering to it. An `origin:name` entry has to match a row's
/// exact key — comparing it by bare name would make `mcp:github:report` look live
/// merely because some *local* `report` exists, so an entry left behind by a
/// deleted server or a renamed one could never be cleaned up.
///
/// Unparseable entries are stale by definition — there is nothing they could
/// have meant.
fn stale_disabled(raw: &[String], rows: &[SkillSummary]) -> Vec<String> {
    raw.iter()
        .filter(|entry| {
            let Some(key) = qcode_tools::skills::DisableKey::parse(entry) else {
                return true;
            };
            match &key.origin {
                None => !rows.iter().any(|row| row.name == key.name),
                Some(_) => !rows.iter().any(|row| row.key == key.render()),
            }
        })
        .cloned()
        .collect()
}

impl SetupView {
    /// Build the index the list reads, or adopt the live one.
    ///
    /// `live` is `Some` in the GUI settings window: the running session's
    /// index, which is the only place skills served over MCP exist (ADR-0108
    /// §2). Without it the view discovers locally, exactly like `qc skills
    /// list`, and every skill on screen is a disk skill.
    pub(crate) fn build_skill_index(
        config: &Config,
        workspace: PathBuf,
        live: Option<Arc<SkillIndex>>,
    ) -> (Arc<SkillIndex>, bool) {
        if let Some(index) = live {
            return (index, true);
        }
        let (set, skipped) = qcode_kernel::load_skills_reporting(&config.skills, &workspace);
        let index = SkillIndex::with_disabled((*set).clone(), &config.skills.disabled)
            .with_skipped(skipped);
        (Arc::new(index), false)
    }

    /// Re-read the rows when the index moved. Cheap enough to call per render:
    /// the comparison is one atomic load.
    pub(crate) fn refresh_skill_rows(&mut self) {
        let revision = self.skills.revision();
        if revision == self.skill_rows_revision && !self.skill_rows.is_empty() {
            return;
        }
        self.skill_rows_revision = revision;
        self.skill_rows = self.skills.summaries();
        self.stale_skill_keys = stale_disabled(&self.config.skills.disabled, &self.skill_rows);
    }

    /// Turn one skill on or off (ADR-0110 §5).
    ///
    /// Two things happen together on purpose: the file is written, and the
    /// live index is told. Writing alone would leave the row reading the old
    /// state until the kernel rebuild finished, i.e. a switch that appears to
    /// ignore the click.
    pub(crate) fn set_skill_enabled(&mut self, key: &str, enabled: bool, cx: &mut Context<Self>) {
        let keys = toggle_disabled(&self.config.skills.disabled, key, enabled);
        self.config.skills.disabled = keys.clone();
        // The view's own index *is* the live one when a kernel is assembled, so
        // this covers both data sources.
        self.skills.set_disabled(&keys);
        self.skill_rows_revision = u64::MAX; // force a refresh on the next render
        self.auto_save(cx, AutoSaveKind::Scalar(&["skills", "disabled"]), false);
        cx.notify();
    }

    /// Drop every disabled entry that matches nothing.
    ///
    /// Uses the same predicate as the badge above, so 「有 N 条没有对应的技能」
    /// and 「清理」 can never disagree about which ones they are.
    pub(crate) fn clear_stale_skill_keys(&mut self, cx: &mut Context<Self>) {
        let stale = stale_disabled(&self.config.skills.disabled, &self.skill_rows);
        let keys: Vec<String> = self
            .config
            .skills
            .disabled
            .iter()
            .filter(|raw| !stale.contains(raw))
            .cloned()
            .collect();
        self.set_skill_enabled_many(keys, cx);
    }

    fn set_skill_enabled_many(&mut self, keys: Vec<String>, cx: &mut Context<Self>) {
        self.config.skills.disabled = keys.clone();
        self.skills.set_disabled(&keys);
        self.skill_rows_revision = u64::MAX;
        self.auto_save(cx, AutoSaveKind::Scalar(&["skills", "disabled"]), false);
        cx.notify();
    }

    /// Open the content dialog for `key`. Unfiltered on purpose: reading a
    /// skill before deciding to keep it off is half the point of this page.
    pub(crate) fn open_skill_by_key(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(skill) = self.skills.detail(key) {
            self.open_skill = Some(OpenSkill {
                key: key.to_string(),
                skill,
                copied_at: None,
            });
        }
        cx.notify();
    }

    /// Close the content dialog. Returns whether one was open, so the Escape
    /// handler can tell 「close the dialog」 from 「close the window」.
    pub(crate) fn dismiss_open_skill(&mut self, cx: &mut Context<Self>) -> bool {
        let had = self.open_skill.take().is_some();
        if had {
            cx.notify();
        }
        had
    }

    /// The roots this view checks, for the empty state.
    fn skill_roots_here(&self) -> Vec<(String, String)> {
        qcode_kernel::skill_roots(&self.config.skills, &self.skills_workspace)
            .into_iter()
            .map(|(source, path)| (source.label().to_string(), path.display().to_string()))
            .collect()
    }

    /// The 技能 page's list card (ADR-0110 §6).
    pub(crate) fn render_skill_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.refresh_skill_rows();

        if !self.config.skills.enabled {
            return div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(crate::sp(crate::CAPTION_SIZE))
                        .text_color(cx.theme().muted_foreground)
                        .child(qcode_i18n::tr("gui.skills.disabled_source")),
                )
                .into_any_element();
        }

        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .min_w_0()
            .child(div().flex_grow(1.0).min_w_0().child(qcode_i18n::trp(
                "gui.skills.header",
                &[self.skill_rows.len().to_string().as_str()],
            )))
            .child(
                div()
                    .text_size(crate::sp(crate::CAPTION_SIZE))
                    .text_color(muted)
                    // A deep workspace path would otherwise push 刷新 off the
                    // card; the row above already says which project this is.
                    .max_w(px(260.0))
                    .truncate()
                    .child(qcode_i18n::trp(
                        "gui.skills.workspace",
                        &[self.skills_workspace.display().to_string().as_str()],
                    )),
            )
            .child(
                Button::new("skills-refresh")
                    .label(qcode_i18n::tr("gui.skills.refresh"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if this.skills_live {
                            // The live index refreshes itself on the server's
                            // ttl; the button only re-reads it.
                            this.skill_rows_revision = u64::MAX;
                        } else {
                            let (index, live) = SetupView::build_skill_index(
                                &this.config,
                                this.skills_workspace.clone(),
                                None,
                            );
                            this.skills = index;
                            this.skills_live = live;
                            this.skill_rows_revision = u64::MAX;
                        }
                        cx.notify();
                    })),
            );

        let mut card = div().flex().flex_col().gap_2().pt_2().w_full().min_w_0();
        card = card.child(header);

        if self.skill_rows.is_empty() {
            let mut empty = div().flex().flex_col().gap_1().child(
                div()
                    .text_size(crate::sp(crate::CAPTION_SIZE))
                    .text_color(muted)
                    .child(qcode_i18n::tr("gui.skills.empty")),
            );
            for (source, path) in self.skill_roots_here() {
                empty = empty.child(
                    div()
                        .text_size(crate::sp(crate::CAPTION_SIZE))
                        .text_color(muted)
                        .child(format!("[{source}] {path}")),
                );
            }
            card = card.child(empty);
        } else {
            // The list scrolls itself instead of pushing the three settings
            // fields above it off the page. `.id()` is load-bearing:
            // `overflow_y_scroll` needs a stateful element.
            let mut list = div()
                .id("skills-list-scroll")
                .flex()
                .flex_col()
                .gap_1()
                .w_full()
                .min_w_0()
                .when(self.skill_rows.len() > 8, |this| {
                    this.max_h(px(360.0)).overflow_y_scroll()
                });
            for row in self.skill_rows.clone() {
                list = list.child(self.render_skill_row(&row, cx));
            }
            card = card.child(
                div()
                    .border_t_1()
                    .border_color(border)
                    .pt_2()
                    .w_full()
                    .min_w_0()
                    .child(list),
            );
        }

        if !self.skills.skipped().is_empty() {
            let mut skipped = div().flex().flex_col().gap_1().child(
                div()
                    .text_size(crate::sp(crate::CAPTION_SIZE))
                    .text_color(cx.theme().danger)
                    .child(qcode_i18n::trp(
                        "gui.skills.skipped",
                        &[self.skills.skipped().len().to_string().as_str()],
                    )),
            );
            for skip in self.skills.skipped() {
                skipped = skipped.child(
                    div()
                        .text_size(crate::sp(crate::CAPTION_SIZE))
                        .text_color(muted)
                        .child(format!("{} — {}", skip.path.display(), skip.reason)),
                );
            }
            card = card.child(skipped);
        }

        if !self.stale_skill_keys.is_empty() {
            card = card.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_grow(1.0)
                            .text_size(crate::sp(crate::CAPTION_SIZE))
                            .text_color(muted)
                            .child(qcode_i18n::trp(
                                "gui.skills.stale",
                                &[self.stale_skill_keys.len().to_string().as_str()],
                            )),
                    )
                    .child(
                        Button::new("skills-clear-stale")
                            .label(qcode_i18n::tr("gui.skills.clear_stale"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.clear_stale_skill_keys(cx);
                            })),
                    ),
            );
        }

        card.into_any_element()
    }

    /// One row: switch, identity, and (for a local skill) where it lives.
    ///
    /// The description and path stay on the row rather than behind the dialog:
    /// choosing which of forty skills to keep is a scanning task, and a dialog
    /// per row makes it forty clicks.
    fn render_skill_row(&self, row: &SkillSummary, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, foreground, border) = (theme.muted_foreground, theme.foreground, theme.border);
        // A served skill gets a weaker badge: 「this is not from your disk」 is
        // the one thing about it that matters at a glance (ADR-0108 §3).
        let badge_color = if row.served { muted } else { foreground };
        let key = row.key.clone();
        let open_key = row.key.clone();
        let toggle_key = row.key.clone();
        let enabled = !row.disabled;
        let description = row
            .description
            .replace(['\n', '\r'], " ")
            .chars()
            .take(160)
            .collect::<String>();
        let files = qcode_i18n::trp("gui.skills.files", &[row.files.len().to_string().as_str()]);
        let location = row.location.clone();
        let name_color = if row.disabled { muted } else { foreground };

        div()
            .flex()
            .flex_row()
            .items_start()
            .gap_3()
            .py_1()
            .border_b_1()
            .border_color(border)
            .child(
                div().pt(px(2.0)).child(
                    Switch::new(format!("skill-switch-{key}"))
                        .checked(enabled)
                        .on_change({
                            let entity = cx.entity();
                            move |checked, _window, cx| {
                                let checked = *checked;
                                let key = toggle_key.clone();
                                entity.update(cx, |view, cx| {
                                    view.set_skill_enabled(&key, checked, cx);
                                });
                            }
                        }),
                ),
            )
            .child(
                // `min_w_0` is load-bearing: a flex item's min-width defaults to
                // its content, so without it a long description pushes the row
                // wider than the card and the lines overlap each other.
                div()
                    .flex_grow(1.0)
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .id(format!("skill-row-{key}"))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.open_skill_by_key(&open_key, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().text_color(name_color).child(row.name.clone()))
                            .child(
                                div()
                                    .text_size(crate::sp(crate::CAPTION_SIZE))
                                    .text_color(badge_color)
                                    .child(row.source.clone()),
                            )
                            .child(
                                div()
                                    .text_size(crate::sp(crate::CAPTION_SIZE))
                                    .text_color(muted)
                                    .child(files),
                            ),
                    )
                    .child(
                        div()
                            .text_size(crate::sp(crate::CAPTION_SIZE))
                            .text_color(muted)
                            .child(description),
                    )
                    .child(
                        // A path has no spaces, so wrapping it is unreadable and
                        // it would push the row width around; truncate instead.
                        div()
                            .text_size(crate::sp(crate::CAPTION_SIZE))
                            .text_color(muted)
                            .truncate()
                            .child(location),
                    ),
            )
            .into_any_element()
    }

    /// The content dialog (ADR-0110 §7): scrollable body, 复制, Esc to close.
    pub(crate) fn render_open_skill_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(open) = self.open_skill.as_ref() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (popover, popover_fg, muted, border) = (
            theme.popover,
            theme.popover_foreground,
            theme.muted_foreground,
            theme.border,
        );
        let key = open.key.clone();
        let name = open.skill.name.clone();
        let source = open.skill.source.origin_label();
        let meta = format!(
            "{} · {} files",
            open.skill.dir.location(),
            open.skill.files.len()
        );
        let copied = open
            .copied_at
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(1500));
        let body = open.skill.body.clone();
        let served = open.skill.body_is_remote();
        let copy_label = if copied {
            qcode_i18n::tr("gui.skills.dialog.copied")
        } else {
            qcode_i18n::tr("gui.skills.dialog.copy")
        };

        // The panel is a *child* variable, not `dialog.child(…)`: the overlay is
        // a centered flex container, so anything appended to it lands as a
        // second centred item instead of inside the panel. That mistake put the
        // body over the whole window with the buttons stranded in a corner.
        let mut panel = div()
            .w(px(760.0))
            .max_h(relative(0.85))
            .flex()
            .flex_col()
            .gap_3()
            .rounded(px(10.0))
            .bg(popover)
            .border_1()
            .border_color(border)
            .p_4()
            .text_color(popover_fg)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_grow(1.0)
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(name)
                            .child(
                                div()
                                    .text_size(crate::sp(crate::CAPTION_SIZE))
                                    .text_color(muted)
                                    .child(source.clone()),
                            ),
                    )
                    .child(
                        Button::new("skill-copy")
                            .label(copy_label)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.copy_open_skill_body(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("skill-close")
                            .label(qcode_i18n::tr("gui.skills.dialog.close"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.dismiss_open_skill(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(crate::sp(crate::CAPTION_SIZE))
                    .text_color(muted)
                    .child(meta),
            );

        if served {
            // Served skills carry no body on this side: SEP-2640 forbids
            // fetching a skill's files ahead of need (ADR-0108 §4).
            panel = panel
                .child(
                    div()
                        .id("skill-view-scroll")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .child(TextView::markdown(
                            "skill-view-served",
                            open.skill.description.clone(),
                        )),
                )
                .child(
                    div()
                        .text_size(crate::sp(crate::CAPTION_SIZE))
                        .text_color(muted)
                        .child(qcode_i18n::trp("gui.skills.dialog.served", &[&source])),
                );
        } else if body.trim().is_empty() {
            panel = panel.child(
                div()
                    .text_size(crate::sp(crate::CAPTION_SIZE))
                    .text_color(muted)
                    .child(qcode_i18n::tr("gui.skills.dialog.empty_body")),
            );
        } else {
            // `min_h(0)` on the scroller and `min_h(0)` on the panel are what let
            // `max_h` win: without them a long `SKILL.md` grows the panel past
            // the window and there is nothing left to scroll. `w_full` +
            // `min_w_0` keep a wide code block from stretching the panel.
            panel = panel.child(
                div()
                    .id("skill-view-scroll")
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .min_w_0()
                    .overflow_y_scroll()
                    .child(TextView::markdown("skill-view-body", body).w_full()),
            );
        }

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            // `key` keeps the dialog identity stable across re-renders.
            .child(panel.id(format!("skill-dialog-{key}")))
            .into_any_element()
    }

    /// Copy the *raw* `SKILL.md` body.
    ///
    /// Not `copy_text::plain_text_from_markdown`: that flattens markdown into
    /// chat-bubble prose, and a skill's value is exactly the structure it would
    /// throw away.
    fn copy_open_skill_body(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open_skill.as_mut() else {
            return;
        };
        if open.skill.body.is_empty() {
            return;
        }
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(open.skill.body.clone()));
        open.copied_at = Some(std::time::Instant::now());
    }
}

#[cfg(test)]
mod tests;
