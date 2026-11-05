//! Models section card UI (settings mode, design SETTINGS-MODELS-UI):
//! provider row cards with credential dots, one inline editor at a time, the
//! model catalog editor, the probe candidate picker and the delete
//! confirmation. Wizard mode keeps the original table form in `render.rs`.

use super::*;
use crate::{CAPTION_SIZE, SECONDARY_SIZE, sp};
use gpui_kit::component::input::Textarea;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::ButtonVariants as _;

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
            .or_else(|| qcode_kernel::config::catalog_entry(&row.route).map(|entry| entry.base_url));
        let Some(base) = base else {
            return false;
        };
        return crate::keyless_endpoint_reachable(base);
    }
    true
}

impl SetupView {
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
        // default is summarized here and pinned per card via "设为默认".
        let default_line = match (
            self.config.default_model.provider.as_deref(),
            self.config.default_model.model.as_deref(),
        ) {
            (Some(provider), Some(model)) => format!("当前默认：{provider} / {model}"),
            (Some(provider), None) => format!("当前默认提供方：{provider}"),
            _ => "未设置默认；将使用第一个可用的提供方。".to_string(),
        };
        cards = cards.child(
            div()
                .text_size(sp(SECONDARY_SIZE))
                .text_color(muted_color)
                .child(default_line),
        );
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
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.make_default(&route_for_default, cx);
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
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.make_default(&route_for_default, cx);
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
                Input::new(input)
                    .aria_label(column.label)
                    .into_any_element()
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
            .filter(|id| query.is_empty() || id.to_lowercase().contains(&query))
            .collect::<Vec<_>>();
        let all_visible_picked =
            !visible.is_empty() && visible.iter().all(|id| self.probe_picked.contains(*id));
        let entity = cx.entity();
        let visible_ids = visible.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let mut list = div()
            .id("probe-candidate-list")
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(320.0))
            .overflow_y_scroll();
        for id in &visible_ids {
            let checked = self.probe_picked.contains(id);
            let id_clone = id.clone();
            let entity = entity.clone();
            list = list.child(
                Checkbox::new(format!("probe-candidate-{id}"))
                    .checked(checked)
                    .label(id.clone())
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
    fn make_default(&mut self, route: &str, cx: &mut Context<Self>) {
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
        self.config.default_model.provider = Some(route.to_string());
        self.config.default_model.model = model;
        self.config.default_provider = None;
        match super::save::write_config(&self.path, &self.config) {
            Ok(()) => self.saved_notice = Some(format!("已设 {route} 为默认。")),
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
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
