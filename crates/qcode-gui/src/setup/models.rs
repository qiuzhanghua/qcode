//! Models section card UI (settings mode, design SETTINGS-MODELS-UI):
//! provider row cards with credential dots, one inline editor at a time, the
//! model catalog editor, the probe candidate picker and the delete
//! confirmation. Wizard mode keeps the original table form in `render.rs`.

use super::*;
use crate::{CAPTION_SIZE, SECONDARY_SIZE, sp};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::Textarea;
use gpui_kit::component::popover::Popover;

/// `^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$`: a route id usable as a settings key.
fn route_valid(route: &str) -> bool {
    let mut chars = route.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    let mut after_dash = false;
    for c in chars {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            after_dash = false;
        } else if c == '-' && !after_dash {
            after_dash = true;
        } else {
            return false;
        }
    }
    !after_dash
}

fn credential_text(state: qcode_kernel::provider_registry::CredentialState) -> &'static str {
    match state {
        qcode_kernel::provider_registry::CredentialState::Configured => "凭证已配置",
        qcode_kernel::provider_registry::CredentialState::Missing => "凭证缺失",
        qcode_kernel::provider_registry::CredentialState::Keyless => "无需密钥",
    }
}

/// 一个 provider 是否真正可用：凭证齐全或无需密钥，且无需密钥的
/// provider（如 ollama）端点必须可达，否则视为不可用——不可用的
/// provider 不给「设为默认」入口。
fn provider_usable(
    config: &Config,
    row: &qcode_kernel::provider_registry::ConfigurableProvider,
) -> bool {
    use qcode_kernel::provider_registry::CredentialState;
    if matches!(row.credential_state, CredentialState::Missing) {
        return false;
    }
    if matches!(row.credential_state, CredentialState::Keyless) {
        let base = config
            .providers
            .get(&row.route)
            .and_then(|profile| profile.base_url.as_deref())
            .or_else(|| {
                qcode_kernel::config::catalog_entry(&row.route).map(|entry| entry.base_url)
            });
        let Some(base) = base else {
            return false;
        };
        return crate::keyless_endpoint_reachable(base);
    }
    true
}

/// 默认模型选择器的候选：按 provider 分组，来自**内存里的草稿配置**。
///
/// 与 `crate::provider_models` 有两点不同，都是刻意的：
///
/// - 读 `config` 而不是磁盘上的 config.toml —— 设置页正在编辑的就是这份草稿，
///   改了 provider 的模型目录还没落盘时，选择器就该看得见。
/// - 不按凭证/端点可用性过滤。设置页正是补凭证的地方；把 provider 从列表里
///   藏起来，用户会以为自己的修改没保存。真正不能用的 provider 在跑任务时
///   才会报错，那时错误信息比列表里少一行有用得多。
pub(super) fn default_model_candidates(config: &Config) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for (name, provider) in &config.providers {
        // mock 路由不是真 provider，不该被选成默认。
        if matches!(
            name.as_str(),
            "mock" | "mock-tools" | "mock-approval" | "mock-todo" | "mock-subagent"
        ) {
            continue;
        }
        let mut models: Vec<String> = provider
            .models
            .iter()
            .map(|model| model.id.clone())
            .collect();
        if let Some(default) = &provider.default_model
            && !models.iter().any(|model| model == default)
        {
            models.push(default.clone());
        }
        if models.is_empty() {
            models = config::catalog_models(name)
                .into_iter()
                .map(|model| model.id)
                .collect();
        }
        if !models.is_empty() {
            out.push((name.clone(), models));
        }
    }
    out
}

/// `pin_default` 的内存部分：把 provider 与 model 写进 `[default-model]`，
/// **并**写进 `providers.<provider>.default_model`。
///
/// 两处都写不是冗余：`apply_tables` 里 `sync_default_model` 用行值覆盖
/// `[default-model]`，只写后者的化，用户下次改任何表格字段都会把刚选的模型
/// 改回去。抽成纯函数是为了能在无窗口的测试里钉住这条不变量。
pub(super) fn pin_default_fields(config: &mut Config, provider: &str, model: Option<&str>) {
    config.default_model.provider = Some(provider.to_string());
    config.default_model.model = model.map(str::to_string);
    // V2.1：遗留键永不写新值，留着会与 [default-model] 争默认。
    config.default_provider = None;
    if let Some(model) = model
        && let Some(entry) = config.providers.get_mut(provider)
    {
        entry.default_model = Some(model.to_string());
    }
}

/// What a candidate row is called: the id, plus the provider's display name
/// when it differs. The id is what gets written to the config; the name is what
/// the provider's documentation calls the model.
fn candidate_label(candidate: &qcode_providers::discovery::ModelCandidate) -> String {
    match candidate.name.as_deref() {
        Some(name) if name != candidate.id => format!("{} — {name}", candidate.id),
        _ => candidate.id.clone(),
    }
}

/// The warning under the free-text model box, or `None` when the name is known.
///
/// Typing a display name where an id belongs is a real and quiet mistake: the
/// config saves, the first request comes back 400, and the model name looks
/// correct because it is — just not as an id.
fn unknown_model_message(typed: &str, known: &[String]) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() || known.iter().any(|id| id == typed) {
        return None;
    }
    Some(format!(
        "「{typed}」不在已知的模型列表里。这里填的是 id，不是显示名（例如 deepseek-flash，而不是 DeepSeek-V4.1-Flash）；可以点卡片上的「获取可用模型」核对。"
    ))
}

/// Every model id the settings window already knows for `provider`: the rows in
/// `[[providers.<p>.models]]`, that provider's `default_model`, the pinned
/// `[default-model]` when it points here, and the builtin catalog.
///
/// Used only to warn when the free-text model box is given something that is
/// not an id at all — the box still accepts it, because a brand-new model or a
/// gateway alias has to be typeable before the catalog catches up.
fn known_model_ids(config: &Config, provider: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    if let Some(profile) = config.providers.get(provider) {
        ids.extend(profile.models.iter().map(|model| model.id.clone()));
        if let Some(model) = &profile.default_model {
            ids.push(model.clone());
        }
    }
    if config.default_model.provider.as_deref() == Some(provider)
        && let Some(model) = &config.default_model.model
    {
        ids.push(model.clone());
    }
    ids.extend(
        qcode_kernel::config::catalog_models(provider)
            .into_iter()
            .map(|model| model.id),
    );
    ids.retain(|id| !id.is_empty());
    ids.sort();
    ids.dedup();
    ids
}

impl SetupView {
    /// 顶部那行「当前默认：provider / model」，现在可点：展开一个按 provider
    /// 分组的候选列表，选中即写 `[default-model]`。
    ///
    /// 加这个入口之前，设置页**没有**改默认模型的途径：每张卡片的「设为默认」
    /// 只改 provider（已经是默认时不渲染），而 provider 行里的「Default
    /// model」被 `[default-model].model` 压着（见 `sync_default_model`）。
    /// 于是「换同一家的另一个模型」这件事只能在底部模型菜单里做，或者手改
    /// config.toml。这里把它放回设置页。
    ///
    /// 候选来自内存里的草稿配置而不是磁盘：正在编辑的就是它。也不按凭证过滤
    /// ——设置页正是补凭证的地方，把 provider 藏起来只会让人以为它没保存。
    fn render_default_model_line(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // 「手输」是 id 与显示名最容易混起来的地方（DeepSeek 的 id 是
        // `deepseek-flash`，显示名是 `DeepSeek-V4.1-Flash`）：打了一个已知
        // 清单里没有的名字时给一句提示，而不是等用户在请求里拿到 400。
        // 计算放在构建弹层之前，闭包就不必再借 `self`。
        let unknown_model_hint: Option<String> = {
            let typed = self.default_model_input.read(cx).value().trim().to_string();
            match self.config.default_model.provider.clone() {
                Some(provider) => {
                    unknown_model_message(&typed, &known_model_ids(&self.config, &provider))
                }
                None => None,
            }
        };
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hovered = theme.secondary;
        let provider = self.config.default_model.provider.clone();
        let model = self.config.default_model.model.clone();
        let label = match (&provider, &model) {
            (Some(provider), Some(model)) => format!("当前默认：{provider} / {model}"),
            (Some(provider), None) => format!("当前默认提供方：{provider}"),
            _ => "未设置默认；将使用第一个可用的提供方。".to_string(),
        };
        let groups = default_model_candidates(&self.config);
        // 目标 provider：当前默认的那个；没有默认时取候选里的第一个，否则用户
        // 输入的模型无处可去。
        let target_provider = provider
            .clone()
            .or_else(|| groups.first().map(|(name, _)| name.clone()));
        let shell = cx.entity().downgrade();
        let input = self.default_model_input.clone();

        Popover::new("default-model-popover")
            .anchor(Anchor::BottomLeft)
            .appearance(true)
            .overlay_closable(true)
            .trigger(
                Button::new("default-model-trigger")
                    .label(label)
                    .text()
                    .dropdown_caret(true),
            )
            .content(move |_state, _window, cx| {
                let state = cx.entity().downgrade();
                let mut rows = div().flex().flex_col().gap_1().p_1().min_w(px(240.0));
                if groups.is_empty() {
                    rows = rows.child(
                        div()
                            .p_2()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(muted)
                            .child("还没有可选的模型：先给一个 provider 配上模型或默认模型。"),
                    );
                }
                for (group_provider, models) in &groups {
                    rows = rows.child(
                        div()
                            .h(px(22.0))
                            .px_2()
                            .flex()
                            .items_center()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(muted)
                            .child(group_provider.clone()),
                    );
                    for group_model in models {
                        let checked = provider.as_deref() == Some(group_provider.as_str())
                            && model.as_deref() == Some(group_model.as_str());
                        let p = group_provider.clone();
                        let m = group_model.clone();
                        let shell = shell.clone();
                        let state = state.clone();
                        rows = rows.child(
                            div()
                                .id(format!("default-model-row-{group_provider}-{group_model}"))
                                .h(px(24.0))
                                .px_2()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .rounded_sm()
                                .cursor_pointer()
                                .text_size(sp(SECONDARY_SIZE))
                                .hover(move |this| this.bg(hovered))
                                .child(div().flex_1().child(m.clone()))
                                .when(checked, |this| this.child("✓"))
                                .on_click(move |_event, window, cx| {
                                    if let Some(shell) = shell.upgrade() {
                                        shell.update(cx, |view, cx| {
                                            view.pin_default(&p, Some(m.as_str()), window, cx);
                                        });
                                    }
                                    if let Some(state) = state.upgrade() {
                                        state.update(cx, |state, cx| state.dismiss(window, cx));
                                    }
                                }),
                        );
                    }
                }
                // 目录里没有的模型（新版本、网关别名）只能靠手输：候选列表
                // 只会列出已经登记的值，没有这一行就「设不成功」。
                //
                let free_text = div()
                    .mt_1()
                    .pt_2()
                    .border_t_1()
                    .border_color(muted)
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_size(sp(CAPTION_SIZE)).text_color(muted).child(
                        match &target_provider {
                            Some(provider) => format!("设为 {provider} 的默认模型"),
                            None => "先配置一个 provider".to_string(),
                        },
                    ))
                    .child(Input::new(&input).aria_label("默认模型名"))
                    .child(match &unknown_model_hint {
                        Some(message) => div()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(muted)
                            .child(message.clone())
                            .into_any_element(),
                        None => div().into_any_element(),
                    })
                    .child(
                        Button::new("set-custom-default-model")
                            .label("设为默认")
                            .on_click({
                                let shell = shell.clone();
                                let state = state.clone();
                                let input = input.clone();
                                let target = target_provider.clone();
                                move |_event, window, cx| {
                                    let Some(provider) = target.clone() else {
                                        return;
                                    };
                                    let typed = input.read(cx).value().trim().to_string();
                                    if typed.is_empty() {
                                        return;
                                    }
                                    if let Some(shell) = shell.upgrade() {
                                        shell.update(cx, |view, cx| {
                                            view.pin_default(
                                                &provider,
                                                Some(typed.as_str()),
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                    input.update(cx, |input, cx| {
                                        input.set_value("", window, cx);
                                        cx.notify();
                                    });
                                    if let Some(state) = state.upgrade() {
                                        state.update(cx, |state, cx| state.dismiss(window, cx));
                                    }
                                }
                            }),
                    );
                rows = rows.child(free_text);
                rows.into_any_element()
            })
            .into_any_element()
    }

    /// The provider card list: saved notice, one card per directory row, the
    /// inline editor for the open route, and the add actions.
    pub(crate) fn render_models_providers(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let directory = qcode_kernel::provider_registry::ProviderRegistry::new(None)
            .list_configurable_providers(&self.config);
        let theme = cx.theme();
        let (success_color, muted_color, border_color) =
            (theme.success, theme.muted_foreground, theme.border);
        let mut cards = div().flex().flex_col().gap_2();
        // V2.1: the three legacy default fields were removed; the effective
        // default is summarized here (and changeable by clicking it) and
        // pinned per card via "设为默认".
        cards = cards.child(self.render_default_model_line(cx));
        if let Some(notice) = &self.saved_notice {
            cards = cards.child(
                div()
                    .text_size(sp(SECONDARY_SIZE))
                    .text_color(success_color)
                    .child(notice.to_string()),
            );
        }
        for row in &directory {
            let route = row.route.clone();
            let display = if row.display_name == row.route {
                row.route.clone()
            } else {
                format!("{} ({})", row.display_name, row.route)
            };
            let dot = match row.credential_state {
                qcode_kernel::provider_registry::CredentialState::Configured => {
                    Some(div().size(px(8.0)).rounded_full().bg(success_color))
                }
                // 未配置凭据用空心圆圈（透明填充、中性色描边），不用红点。
                qcode_kernel::provider_registry::CredentialState::Missing => Some(
                    div()
                        .size(px(8.0))
                        .rounded_full()
                        .border_1()
                        .border_color(muted_color),
                ),
                qcode_kernel::provider_registry::CredentialState::Keyless => None,
            };
            let mut head = div().flex().flex_row().items_center().gap_2();
            if let Some(dot) = dot {
                head = head.child(dot);
            }
            head = head
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().child(display))
                        .child(
                            div()
                                .text_size(sp(CAPTION_SIZE))
                                .text_color(muted_color)
                                .child(format!(
                                    "{} · {}",
                                    row.protocol.label(),
                                    credential_text(row.credential_state)
                                )),
                        ),
                )
                .child(div().flex_grow(1.0));
            if self.config.default_model.provider.as_deref() == Some(route.as_str()) {
                head = head.child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(success_color)
                        .child("当前默认"),
                );
            }
            // 已配置且可用（凭证齐全或无需密钥且端点可达）的 provider，只要
            // 还不是默认，就在卡片上给一个「设为默认」快捷按钮。
            let usable = provider_usable(&self.config, row);
            let is_default = self.config.default_model.provider.as_deref() == Some(route.as_str());
            if row.configured && usable && !is_default {
                let route_for_default = route.clone();
                head = head.child(
                    Button::new(format!("default-provider-{route}"))
                        .label("设为默认")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.make_default(&route_for_default, window, cx);
                        })),
                );
            }
            // 编辑在弹出对话框里进行（删除等动作也移入对话框），卡片只放
            // 编辑/添加按钮。未配置的提供方首次点击会先写入预设行再弹出
            // 同一个编辑对话框。
            if row.configured {
                let route_for_edit = route.clone();
                head = head.child(
                    Button::new(format!("edit-provider-{route}"))
                        .label("编辑")
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.open_editor(&route_for_edit, cx);
                        })),
                );
            } else {
                let route_for_add = route.clone();
                head = head.child(
                    Button::new(format!("add-provider-{route}"))
                        .label("编辑")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.add_provider_preset(&route_for_add, window, cx);
                            this.open_editor(&route_for_add, cx);
                        })),
                );
            }
            cards = cards.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_1()
                    .border_color(border_color)
                    .rounded(px(10.0))
                    .p_3()
                    .child(head),
            );
        }
        cards = cards.child(
            div().flex().flex_row().gap_2().child(
                Button::new("open-custom-provider")
                    .label("＋ 添加自定义提供方")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_custom_card(window, cx);
                    })),
            ),
        );
        cards.into_any_element()
    }

    /// The provider edit modal (settings mode): API key first, the fixed
    /// config values and the model catalog collapsed behind an expand toggle.
    /// One provider at a time; the card list only carries one "编辑/添加"
    /// button. 默认/删除动作也在这里进行。
    pub(crate) fn render_provider_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.wizard {
            return div().into_any_element();
        }
        let Some(route) = &self.editing_provider else {
            return div().into_any_element();
        };
        let Some(table) = self.tables.get("providers") else {
            return div().into_any_element();
        };
        let Some(row) = table
            .rows
            .iter()
            .find(|row| cell(row, "name", cx).trim() == route)
        else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (popover, popover_fg, border_color, danger_color) = (
            theme.popover,
            theme.popover_foreground,
            theme.border,
            theme.danger,
        );
        let directory = qcode_kernel::provider_registry::ProviderRegistry::new(None)
            .list_configurable_providers(&self.config);
        let entry = directory.iter().find(|row| row.route == *route);
        let display = entry
            .map(|row| {
                if row.display_name == row.route {
                    row.route.clone()
                } else {
                    format!("{} ({})", row.display_name, row.route)
                }
            })
            .unwrap_or_else(|| route.clone());
        let configured = entry.map(|row| row.configured).unwrap_or(false);
        let usable = entry
            .map(|row| provider_usable(&self.config, row))
            .unwrap_or(false);
        let is_default = self.config.default_model.provider.as_deref() == Some(route.as_str());
        let key = "providers";
        let row_id = row.id;
        let columns = forms::TABLES
            .iter()
            .find(|spec| spec.path.join(".") == key)
            .map(|spec| spec.columns)
            .unwrap_or(&[]);

        let mut body = div().flex().flex_col().gap_3();
        // 第一条是 API 密钥：只有它需要在添加提供方时填写，其余默认都是
        // 固定值，折叠在展开按钮后面。
        if let Some(secret) = row.cells.get("__key") {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .child("API 密钥（存入系统钥匙串，不写入 config.toml）"),
                    )
                    .child(Input::new(secret).aria_label("API 密钥"))
                    .child(
                        Button::new(format!("store-key-{key}-{row_id}"))
                            .label("存入钥匙串")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.store_provider_key(row_id, window, cx);
                            })),
                    ),
            );
        }
        let expanded = self.editor_expanded;
        body = body.child(
            Button::new("toggle-provider-config")
                .label(if expanded {
                    "收起配置 ▴"
                } else {
                    "展开更多配置 ▾"
                })
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.editor_expanded = !this.editor_expanded;
                    cx.notify();
                })),
        );
        // 一行小字错误（如 API 密钥判空），显示在编辑框内而非设置页顶部。
        if let Some(error) = &self.provider_error {
            body = body.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(danger_color)
                    .child(error.to_string()),
            );
        }
        // 蓝色小字成功提示（如密钥已存入钥匙串），同样只在编辑框内显示。
        if let Some(notice) = &self.provider_notice {
            body = body.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(rgb(0x3b82f6))
                    .child(notice.to_string()),
            );
        }
        if expanded {
            let mut fields = div().flex().flex_col().gap_2();
            for column in columns {
                if column.key == "name" {
                    continue;
                }
                if let Some(field) = self.render_provider_field(row, column, cx) {
                    fields = fields.child(field);
                }
            }
            body = body.child(fields);
            body = body.child(self.render_models_editor(row, route, cx));
        }
        if let Some(result) = &self.probe_result {
            body = body.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(danger_color)
                    .child(result.to_string()),
            );
        }

        let mut footer = div().flex().flex_row().items_center().gap_2();
        if configured {
            if usable && !is_default {
                let route_for_default = route.clone();
                footer = footer.child(
                    Button::new(format!("dialog-default-{route}"))
                        .label("设为默认")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.make_default(&route_for_default, window, cx);
                        })),
                );
            }
            let route_for_delete = route.clone();
            footer = footer.child(
                Button::new(format!("dialog-delete-{route}"))
                    .label("删除")
                    .danger()
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.saved_notice = None;
                        this.editing_provider = None;
                        this.delete_target = Some(route_for_delete.clone());
                        cx.notify();
                    })),
            );
        }
        let route_close = route.clone();
        footer = footer
            .child(div().flex_grow(1.0))
            .child(
                Button::new("cancel-provider-edit")
                    .label("取消")
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.close_editor(false);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("save-provider-edit")
                    .label("保存")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // API 密钥输入框的内容只进钥匙串（`__key` 不写
                        // config）：点「保存」时若还有未存入的密钥，先写入
                        // 凭据库并落盘 api_key_command，否则用户只点「保存」
                        // 会以为已生效而实际什么都没存。
                        let pending = this.editing_provider.as_ref().and_then(|route| {
                            let table = this.tables.get("providers")?;
                            table.rows.iter().find_map(|row| {
                                (cell(row, "name", cx).trim() == route.as_str()).then(|| {
                                    let key = row
                                        .cells
                                        .get("__key")
                                        .map(|k| k.read(cx).value().trim().to_string());
                                    (row.id, key)
                                })
                            })
                        });
                        if let Some((row_id, Some(key))) = pending
                            && !key.is_empty()
                        {
                            this.store_provider_key(row_id, window, cx);
                        }
                        this.close_editor(true);
                        this.saved_notice = Some(format!("已保存 {route_close}。"));
                        cx.notify();
                    })),
            );

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(480.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(popover)
                    .border_1()
                    .border_color(border_color)
                    .p_4()
                    .text_color(popover_fg)
                    .child(div().child(format!("编辑 {display}")))
                    .child(
                        div()
                            .id("provider-dialog-scroll")
                            .flex()
                            .flex_col()
                            .gap_3()
                            .max_h(px(440.0))
                            .overflow_y_scroll()
                            .child(body),
                    )
                    .child(footer),
            )
            .into_any_element()
    }

    /// 本行的 `default_model` 是否被 `[default-model].model` 压住（读配置时
    /// 那个字段优先）。判据在 `overridden_default_model` 里，这里只负责把
    /// 行里的值读出来。
    fn overridden_by_global(&self, row: &RowState, cx: &Context<Self>) -> Option<String> {
        let own = cell(row, "default_model", cx);
        let name = cell(row, "name", cx);
        overridden_default_model(&self.config.default_model, &name, &own).map(str::to_string)
    }

    /// One labeled provider column in the edit dialog's expanded section
    /// (protocol / base URL / key env / …). The provider id column is handled
    /// by the dialog itself.
    fn render_provider_field(
        &self,
        row: &RowState,
        column: &forms::ColumnSpec,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let key = "providers";
        let muted_color = cx.theme().muted_foreground;
        let control = match column.kind {
            FieldKind::Bool => Switch::new(format!("table-{key}-{}-{}", row.id, column.key))
                .checked(*row.bools.get(column.key).unwrap_or(&false))
                .on_change({
                    let table_key = key.to_string();
                    let column_key = column.key;
                    let row_id = row.id;
                    let entity = cx.entity();
                    move |checked, _window, cx| {
                        entity.update(cx, |view, cx| {
                            if let Some(table) = view.tables.get_mut(&table_key)
                                && let Some(row) =
                                    table.rows.iter_mut().find(|row| row.id == row_id)
                            {
                                row.bools.insert(column_key, *checked);
                            }
                            view.auto_save(cx, AutoSaveKind::Tables, false);
                            cx.notify();
                        });
                    }
                })
                .into_any_element(),
            FieldKind::Select(options) => {
                let current = row.selects.get(column.key).cloned().unwrap_or_default();
                let entity = cx.entity();
                let mut select = div().flex().flex_row().gap_1();
                for option in options {
                    let marker = if current == *option { "●" } else { "○" };
                    let table_key = key.to_string();
                    let column_key = column.key;
                    let row_id = row.id;
                    let value = option.to_string();
                    let entity = entity.clone();
                    select = select.child(
                        Button::new(format!("table-{key}-{}-{column_key}-{option}", row.id))
                            .label(format!("{marker} {option}"))
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |view, cx| {
                                    if let Some(table) = view.tables.get_mut(&table_key)
                                        && let Some(row) =
                                            table.rows.iter_mut().find(|row| row.id == row_id)
                                    {
                                        row.selects.insert(column_key, value.clone());
                                    }
                                    view.auto_save(cx, AutoSaveKind::Tables, false);
                                    cx.notify();
                                });
                            }),
                    );
                }
                select.into_any_element()
            }
            FieldKind::Lines => {
                let textarea = row.textareas.get(column.key)?;
                Textarea::new(textarea)
                    .h(px(72.0))
                    .aria_label(column.label)
                    .into_any_element()
            }
            _ => {
                let input = row.cells.get(column.key)?;
                let control = Input::new(input)
                    .aria_label(column.label)
                    .into_any_element();
                if column.key != "default_model" {
                    return Some(control);
                }
                // `[default-model].model` 压着这一行（provider 匹配时它优先），
                // 不改 `[default-model]` 的话这里填什么都不生效。保存会把它
                // 同步过去（`sync_default_model`），但改完还没保存的那几秒
                // 必须说清楚现在生效的是哪个，否则又是一次「改了没反应」。
                let mut field = div().flex().flex_col().gap_1().child(control);
                if let Some(global) = self.overridden_by_global(row, cx) {
                    field = field.child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(muted_color)
                            .child(format!("被全局默认「{global}」覆盖；保存后以这里为准")),
                    );
                }
                field.into_any_element()
            }
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(sp(CAPTION_SIZE)).child(column.label))
                .child(control)
                .into_any_element(),
        )
    }

    /// The model catalog editor of one provider: the header (title, inherited
    /// or customized badge, restore/probe actions) and the editable rows.
    fn render_models_editor(
        &self,
        row: &RowState,
        route: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted_color, border_color) = (theme.muted_foreground, theme.border);
        let overridden = !row.models.is_empty();
        let badge = if overridden {
            "已自定义模型目录"
        } else {
            "正在使用适配器默认模型"
        };
        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().child("模型目录"))
            .child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(muted_color)
                    .child(badge),
            )
            .child(div().flex_grow(1.0));
        if overridden {
            let row_id = row.id;
            header = header.child(
                Button::new(format!("reset-models-{row_id}"))
                    .label("恢复默认模型")
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.reset_models(row_id, cx);
                    })),
            );
        }
        let row_id = row.id;
        let route_probe = route.to_string();
        header = header.child(
            Button::new(format!("probe-models-{row_id}"))
                .label("获取可用模型")
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.probe_models(&route_probe, cx);
                })),
        );
        let mut editor = div()
            .flex()
            .flex_col()
            .gap_1()
            .border_t_1()
            .border_color(border_color)
            .pt_2()
            .child(header);
        if overridden {
            for model in &row.models {
                let model_id = model.id;
                let row_id = row.id;
                editor =
                    editor.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_grow(1.5)
                                    .child(Input::new(&model.model_id).aria_label("模型 ID")),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_grow(1.0)
                                    .child(Input::new(&model.name).aria_label("显示名称")),
                            )
                            .child(
                                div().flex().flex_grow(1.0).child(
                                    Input::new(&model.context_window).aria_label("上下文窗口"),
                                ),
                            )
                            .child(
                                div().flex().flex_grow(1.0).child(
                                    Input::new(&model.max_tokens).aria_label("最大输出 token"),
                                ),
                            )
                            .child(
                                Button::new(format!("remove-model-{row_id}-{model_id}"))
                                    .label("×")
                                    .danger()
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.remove_model(row_id, model_id, cx);
                                    })),
                            ),
                    );
            }
            let row_id = row.id;
            editor = editor.child(
                Button::new(format!("add-model-{row_id}"))
                    .label("＋ 添加模型")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_model(row_id, window, cx);
                    })),
            );
        } else {
            let ids = qcode_kernel::config::catalog_models(route)
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>();
            let hint = if ids.is_empty() {
                "模型选择器中将不显示任何模型；目录外 ID 仍可直接发送。".to_string()
            } else {
                format!("{}：{}", badge, ids.join(" / "))
            };
            editor = editor.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(muted_color)
                    .child(hint),
            );
            let row_id = row.id;
            editor = editor.child(
                Button::new(format!("add-model-{row_id}"))
                    .label("＋ 添加模型")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_model(row_id, window, cx);
                    })),
            );
        }
        editor.into_any_element()
    }

    /// The custom-provider declaration dialog (U4): a create, not an edit, so
    /// it asks for the fields nothing can default — route id, endpoint,
    /// protocol — plus display name, key env and the model list. Rendered as a
    /// modal overlay, same as the provider edit dialog.
    pub(crate) fn render_custom_card(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.wizard || !self.declaring {
            return div().into_any_element();
        }
        let theme = cx.theme();
        let (popover, popover_fg, border_color, danger_color) = (
            theme.popover,
            theme.popover_foreground,
            theme.border,
            theme.danger,
        );
        let field = |label: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(sp(CAPTION_SIZE)).child(label))
                .child(Input::new(input).aria_label(label))
        };
        let mut body = div().flex().flex_col().gap_3();
        if let Some(route) = &self.custom_route {
            body = body.child(field("Provider ID", route));
        }
        if let Some(name) = &self.custom_name {
            body = body.child(field("显示名称", name));
        }
        if let Some(base_url) = &self.custom_base_url {
            body = body.child(field("API 地址", base_url));
        }
        if let Some(key_env) = &self.custom_key_env {
            body = body.child(field("Key env", key_env));
        }
        // Protocol choice, like the editor's select buttons.
        let entity = cx.entity();
        let current = self.custom_protocol.clone();
        let mut protocol = div().flex().flex_row().gap_1();
        for option in ["openai", "anthropic"] {
            let marker = if current == option { "●" } else { "○" };
            let value = option.to_string();
            let entity = entity.clone();
            protocol = protocol.child(
                Button::new(format!("custom-protocol-{option}"))
                    .label(format!("{marker} {option}"))
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |view, cx| {
                            view.custom_protocol = value.clone();
                            cx.notify();
                        });
                    }),
            );
        }
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(sp(CAPTION_SIZE)).child("API 协议"))
                .child(protocol),
        );
        // Model drafts: only adopted on create.
        let mut models = div().flex().flex_col().gap_1();
        for model in &self.custom_models {
            let model_id = model.id;
            models = models.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_grow(1.5)
                            .child(Input::new(&model.model_id).aria_label("模型 ID")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_grow(1.0)
                            .child(Input::new(&model.name).aria_label("显示名称")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_grow(1.0)
                            .child(Input::new(&model.context_window).aria_label("上下文窗口")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_grow(1.0)
                            .child(Input::new(&model.max_tokens).aria_label("最大输出 token")),
                    )
                    .child(
                        Button::new(format!("custom-remove-model-{model_id}"))
                            .label("×")
                            .danger()
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.custom_models.retain(|model| model.id != model_id);
                                cx.notify();
                            })),
                    ),
            );
        }
        models = models.child(
            Button::new("custom-add-model")
                .label("＋ 添加模型")
                .on_click(cx.listener(|this, _, window, cx| {
                    let model = this.model_row(window, cx, None);
                    this.custom_models.push(model);
                    cx.notify();
                })),
        );
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(sp(CAPTION_SIZE)).child("模型目录"))
                .child(models),
        );
        if let Some(error) = &self.custom_error {
            body = body.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(danger_color)
                    .child(error.to_string()),
            );
        }
        let footer = div()
            .flex()
            .flex_row()
            .justify_end()
            .gap_2()
            .child(
                Button::new("cancel-custom-provider")
                    .label("取消")
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.close_custom_card();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("create-custom-provider")
                    .label("创建提供方")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.create_custom_provider(window, cx);
                    })),
            );

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(480.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(popover)
                    .border_1()
                    .border_color(border_color)
                    .p_4()
                    .text_color(popover_fg)
                    .child(div().child("添加自定义提供方"))
                    .child(
                        div()
                            .id("custom-provider-dialog-scroll")
                            .flex()
                            .flex_col()
                            .gap_3()
                            .max_h(px(440.0))
                            .overflow_y_scroll()
                            .child(body),
                    )
                    .child(footer),
            )
            .into_any_element()
    }

    /// The probe candidate picker modal (U3): search, select-all/deselect-all
    /// over the visible results, and adopt-on-confirm.
    pub(crate) fn render_probe_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.wizard || self.probe_candidates.is_none() {
            return div().into_any_element();
        }
        if self.probe_search.is_none() {
            let search = input(window, cx, String::new(), "搜索模型");
            self._input_subscriptions.push(cx.subscribe(
                &search,
                |view, _input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        view.probe_query = _input.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ));
            self.probe_search = Some(search);
        }
        let theme = cx.theme();
        let (popover, popover_fg, muted_color, border_color) = (
            theme.popover,
            theme.popover_foreground,
            theme.muted_foreground,
            theme.border,
        );
        let candidates = self.probe_candidates.clone().unwrap_or_default();
        let query = self.probe_query.trim().to_lowercase();
        let visible = candidates
            .iter()
            .filter(|candidate| {
                query.is_empty()
                    || candidate.id.to_lowercase().contains(&query)
                    || candidate
                        .name
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(&query))
            })
            .collect::<Vec<_>>();
        let all_visible_picked = !visible.is_empty()
            && visible
                .iter()
                .all(|candidate| self.probe_picked.contains(&candidate.id));
        let entity = cx.entity();
        let visible_ids = visible
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect::<Vec<_>>();
        let mut list = div()
            .id("probe-candidate-list")
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(320.0))
            .overflow_y_scroll();
        for candidate in &visible {
            let id = &candidate.id;
            let checked = self.probe_picked.contains(id);
            // The label carries both: the id is what gets written to the config,
            // the name is what the provider's own docs call the model. Typing the
            // name where an id belongs is the mistake this line exists to prevent.
            let label = candidate_label(candidate);
            let id_clone = id.clone();
            let entity = entity.clone();
            list = list.child(
                Checkbox::new(format!("probe-candidate-{id}"))
                    .checked(checked)
                    .label(label)
                    .on_change(move |checked, _window, cx| {
                        entity.update(cx, |view, cx| {
                            if *checked {
                                view.probe_picked.insert(id_clone.clone());
                            } else {
                                view.probe_picked.remove(&id_clone);
                            }
                            cx.notify();
                        });
                    }),
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
            .child(
                div()
                    .w(px(460.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(popover)
                    .border_1()
                    .border_color(border_color)
                    .p_4()
                    .text_color(popover_fg)
                    .child(div().child("选择要添加的模型"))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(muted_color)
                            .child("以下是模型提供方的可用模型，勾选要添加的模型。"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                div().flex_grow(1.0).child(
                                    Input::new(self.probe_search.as_ref().unwrap())
                                        .aria_label("搜索模型"),
                                ),
                            )
                            .child(
                                Button::new("probe-toggle-visible")
                                    .label(if all_visible_picked {
                                        "取消全选"
                                    } else {
                                        "全选"
                                    })
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        for id in &visible_ids {
                                            if all_visible_picked {
                                                this.probe_picked.remove(id);
                                            } else {
                                                this.probe_picked.insert(id.clone());
                                            }
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(list)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("close-probe-picker").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.close_probe_picker();
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("adopt-probe-picked")
                                    .label("添加所选")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.adopt_probe_picked(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The provider delete confirmation modal (U5).
    pub(crate) fn render_delete_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(route) = &self.delete_target else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (popover, popover_fg, muted_color, border_color) = (
            theme.popover,
            theme.popover_foreground,
            theme.muted_foreground,
            theme.border,
        );
        let directory = qcode_kernel::provider_registry::ProviderRegistry::new(None)
            .list_configurable_providers(&self.config);
        let display = directory
            .iter()
            .find(|row| row.route == *route)
            .map(|row| {
                if row.display_name == row.route {
                    row.route.clone()
                } else {
                    format!("{} ({})", row.display_name, row.route)
                }
            })
            .unwrap_or_else(|| route.clone());
        let route = route.clone();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(440.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(popover)
                    .border_1()
                    .border_color(border_color)
                    .p_4()
                    .text_color(popover_fg)
                    .child(format!("删除 {display}？"))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(muted_color)
                            .child(format!(
                                "删除 {display} 会移除其配置；已存入系统钥匙串的密钥不会被移除。"
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-delete-provider")
                                    .label("取消")
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.delete_target = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-delete-provider")
                                    .label("删除")
                                    .danger()
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.remove_provider_route(&route, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Pin `route` as the global default (V2.1): write `[default-model]` with
    /// the row's `default_model` (falling back to its first model), and drop
    /// the legacy `default_provider` so it can never shadow the new default.
    fn make_default(&mut self, route: &str, window: &mut Window, cx: &mut Context<Self>) {
        let previous_model = self.config.default_model.model.clone();
        let model = self
            .tables
            .get("providers")
            .and_then(|table| {
                table
                    .rows
                    .iter()
                    .find(|row| cell(row, "name", cx).trim() == route)
            })
            .and_then(|row| {
                let default_model = cell(row, "default_model", cx).trim().to_string();
                if default_model.is_empty() {
                    row.models
                        .first()
                        .map(|model| model.model_id.read(cx).value().trim().to_string())
                        .filter(|id| !id.is_empty())
                } else {
                    Some(default_model)
                }
            });
        // 同一 provider 再点「设为默认」不该丢掉已经钉好的模型（比如从底部
        // 菜单切过的那一个）：没有行内默认时保留旧值。
        let model = model.or_else(|| {
            (self.config.default_model.provider.as_deref() == Some(route))
                .then_some(previous_model)
                .flatten()
        });
        self.pin_default(route, model.as_deref(), window, cx);
    }

    /// 把 `[default-model]` 指向 `provider` / `model` 并立即落盘。设置页顶部
    /// 的选择器和卡片上的「设为默认」都走这里，两条路写的是同一个字段。
    ///
    /// 同时把该 provider 行的 `default_model` 一起改掉：`apply_tables` 里的
    /// `sync_default_model` 会用**行值**覆盖 `[default-model]`，只写后者的话，
    /// 用户下次改任何表格字段都会把刚选的模型改回去。
    pub(crate) fn pin_default(
        &mut self,
        provider: &str,
        model: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        pin_default_fields(&mut self.config, provider, model);
        self.set_row_default_model(provider, model, window, cx);
        let notice = match model {
            Some(model) => format!("已设 {provider} / {model} 为默认。"),
            None => format!("已设 {provider} 为默认提供方。"),
        };
        match super::save::write_config(&self.path, &self.config) {
            Ok(()) => self.saved_notice = Some(notice),
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }

    /// 把 provider 行里的 `default_model` 单元格也改成 `model`，让界面显示与
    /// 配置一致（`Input::set_value` 不触发 `Change`，所以不会引起额外保存）。
    fn set_row_default_model(
        &mut self,
        provider: &str,
        model: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = model else {
            return;
        };
        let Some(input) = self
            .tables
            .get("providers")
            .and_then(|table| {
                table
                    .rows
                    .iter()
                    .find(|row| cell(row, "name", cx).trim() == provider)
            })
            .and_then(|row| row.cells.get("default_model"))
            .cloned()
        else {
            return;
        };
        input.update(cx, |input, cx| {
            input.set_value(model, window, cx);
            cx.notify();
        });
    }

    /// Open one provider's edit dialog; one provider at a time, so opening a
    /// dialog closes the custom declaration card and clears the saved notice.
    fn open_editor(&mut self, route: &str, cx: &mut Context<Self>) {
        self.editing_provider = Some(route.to_string());
        // 每次打开都从折叠态开始：对话框第一条只有 API 密钥。
        self.editor_expanded = false;
        self.provider_error = None;
        self.provider_notice = None;
        self.declaring = false;
        self.saved_notice = None;
        cx.notify();
    }

    /// Close the provider edit dialog. `changed` reports whether the row was
    /// committed (the dialog auto-saves as you type; the flag only decides the
    /// saved-notice copy).
    fn close_editor(&mut self, changed: bool) {
        self.editing_provider = None;
        // 关闭编辑框即清理探测状态与对话框内提示：再次打开时不会残留
        // 上一次的探测结果/报错；探测线程的轮询也会因对话框关闭而丢弃
        // 结果（见 `probe_models`）。
        self.probe_rx = None;
        self.probe_result = None;
        self.probe_candidates = None;
        self.provider_error = None;
        self.provider_notice = None;
        if !changed {
            self.saved_notice = None;
        }
    }

    fn open_custom_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.declaring = true;
        self.editing_provider = None;
        self.saved_notice = None;
        self.custom_route = Some(input(window, cx, String::new(), "Provider ID"));
        self.custom_name = Some(input(window, cx, String::new(), "显示名称"));
        self.custom_base_url = Some(input(window, cx, String::new(), "API 地址"));
        self.custom_key_env = Some(input(window, cx, String::new(), "Key env"));
        self.custom_protocol = "openai".to_string();
        self.custom_models = Vec::new();
        self.custom_error = None;
        cx.notify();
    }

    fn close_custom_card(&mut self) {
        self.declaring = false;
        self.custom_route = None;
        self.custom_name = None;
        self.custom_base_url = None;
        self.custom_key_env = None;
        self.custom_models = Vec::new();
        self.custom_error = None;
    }

    /// Create the hand-declared provider: validate the route id and endpoint,
    /// write a full profile row (protocol + base URL + key env + models), then
    /// open its editor.
    fn create_custom_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let route = self
            .custom_route
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let name = self
            .custom_name
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let base_url = self
            .custom_base_url
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let key_env = self
            .custom_key_env
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if !route_valid(&route) {
            self.custom_error =
                Some("Provider ID 需以小写字母开头，之后可用小写字母、数字和短横线。".to_string());
            cx.notify();
            return;
        }
        let taken = self.config.providers.contains_key(&route)
            || qcode_kernel::config::catalog_entry(&route).is_some()
            || self.tables.get("providers").is_some_and(|table| {
                table
                    .rows
                    .iter()
                    .any(|row| cell(row, "name", cx).trim() == route)
            });
        if taken {
            self.custom_error = Some("已有提供方使用了这个 ID。".to_string());
            cx.notify();
            return;
        }
        if base_url.is_empty() {
            self.custom_error = Some("自定义提供方需要填写 API 地址。".to_string());
            cx.notify();
            return;
        }
        let mut row = self.row(
            [
                ("name", route.clone()),
                ("base_url", base_url.clone()),
                ("api_key_env", key_env),
                ("api_key_command", String::new()),
                ("default_model", String::new()),
                ("extra_body", String::new()),
                ("__key", String::new()),
            ],
            [("stream_usage", true)],
            [("protocol", self.custom_protocol.clone())],
            window,
            cx,
        );
        row.cells
            .insert("__key", secret_input(window, cx, "API key (Keychain)"));
        row.models = std::mem::take(&mut self.custom_models);
        let display_name = if name.is_empty() { route.clone() } else { name };
        if let Some(state) = self.tables.get_mut("providers") {
            state.rows.push(row);
        }
        self.close_custom_card();
        self.editing_provider = Some(route.clone());
        self.saved_notice = Some(format!("已创建 {display_name}。"));
        self.auto_save(cx, AutoSaveKind::Tables, false);
        cx.notify();
    }

    fn close_probe_picker(&mut self) {
        self.probe_candidates = None;
        self.probe_picked.clear();
        self.probe_query.clear();
        self.probe_search = None;
    }
}

#[cfg(test)]
mod label_tests {
    // Named imports on purpose. `setup` glob-imports `gpui_kit::*`, which
    // re-exports GPUI's own `test` proc-macro attribute; `use super::*` would
    // pull it in, and every `#[test]` below would then expand to the GPUI macro
    // instead of the built-in attribute, recursing until the limit trips.
    use super::{candidate_label, unknown_model_message};
    use qcode_providers::discovery::ModelCandidate;

    fn candidate(id: &str, name: Option<&str>) -> ModelCandidate {
        ModelCandidate {
            id: id.to_string(),
            name: name.map(str::to_string),
            ..ModelCandidate::default()
        }
    }

    #[test]
    fn a_display_name_rides_along_with_the_id() {
        assert_eq!(
            candidate_label(&candidate("deepseek-flash", Some("DeepSeek-V4.1-Flash"))),
            "deepseek-flash — DeepSeek-V4.1-Flash"
        );
        // Same name as the id, or no name: the row stays just the id.
        assert_eq!(
            candidate_label(&candidate("deepseek-flash", None)),
            "deepseek-flash"
        );
        assert_eq!(
            candidate_label(&candidate("deepseek-flash", Some("deepseek-flash"))),
            "deepseek-flash"
        );
    }

    #[test]
    fn a_known_id_gets_no_warning() {
        let known = vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()];
        assert_eq!(unknown_model_message("deepseek-flash", &known), None);
        assert_eq!(unknown_model_message("  deepseek-v4-pro ", &known), None);
        assert_eq!(unknown_model_message("", &known), None);
    }

    #[test]
    fn a_display_name_typed_as_an_id_gets_the_id_warning() {
        let known = vec!["deepseek-flash".to_string()];
        let message = unknown_model_message("DeepSeek-V4.1-Flash", &known).unwrap();
        assert!(message.contains("DeepSeek-V4.1-Flash"));
        assert!(message.contains("deepseek-flash"));
        assert!(message.contains("id"));
    }
}
