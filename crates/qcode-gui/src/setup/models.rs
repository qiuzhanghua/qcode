//! Models section card UI (settings mode, design SETTINGS-MODELS-UI):
//! provider row cards with credential dots, one inline editor at a time, the
//! model catalog editor, the probe candidate picker and the delete
//! confirmation. Wizard mode keeps the original table form in `render.rs`.

use super::*;
use crate::{CAPTION_SIZE, CODE_SIZE, SECONDARY_SIZE, sp};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::Textarea;

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

/// 返回 `Cow` 而不是 `&'static str`：译文来自运行期选定的 locale，编译期常量
/// 不再可用。英文命中时仍是借用的常量，不额外分配。
/// 可见于 `setup::render`：向导模式的只读目录与 Provider 卡片显示的是同一套状态，
/// 不该有两份措辞。
pub(crate) fn credential_text(
    state: qcode_kernel::provider_registry::CredentialState,
) -> std::borrow::Cow<'static, str> {
    use qcode_kernel::provider_registry::CredentialState;
    match state {
        CredentialState::Configured => qcode_i18n::tr("gui.setup.credential.configured"),
        CredentialState::Missing => qcode_i18n::tr("gui.setup.credential.missing"),
        CredentialState::Keyless => qcode_i18n::tr("gui.setup.credential.keyless"),
    }
}

impl SetupView {
    /// 一个 provider 是否真正可用：凭证齐全或无需密钥，缺失密钥的 provider
    /// 无法访问、也不能被设为默认。改版后端点可达性不再自动探测：已配置
    /// 模型的 keyless provider（如 ollama）视为可用，真正不可用时由配置页
    /// 「获取可用模型」或任务运行期报错兜底。
    fn provider_usable(&self, row: &qcode_kernel::provider_registry::ConfigurableProvider) -> bool {
        // 太极 / 可配置 OAuth2 统一认证是 keyless provider（OAuth 登录、无 API
        // key）：未登录只是运行任务前的状态，登录随时可补，不因此挡住「设为
        // 默认」；其余 provider 仍按凭证状态判定。
        if qcode_kernel::providers::tj::is_tj_route(&row.route)
            || qcode_kernel::providers::oauth2::is_oauth2_route(&row.route)
        {
            return true;
        }
        !matches!(
            row.credential_state,
            qcode_kernel::provider_registry::CredentialState::Missing
        )
    }
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

/// What a candidate row is called: the vendor's display name (e.g.
/// `deepseek-v4-flash`), falling back to the id when the listing carries no
/// name. The picked value is written into the config as the model id — vendors
/// accept the display name as the model reference.
fn candidate_label(candidate: &qcode_providers::discovery::ModelCandidate) -> String {
    candidate
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| candidate.id.clone())
}

/// 卡片/选择器副标题里的「类型」标签：OAuth2 统一认证显示「其他」（它的 wire
/// protocol 虽是 OpenAI 兼容，但对用户它不是 OpenAI）。
fn provider_type_label(
    row: &qcode_kernel::provider_registry::ConfigurableProvider,
) -> std::borrow::Cow<'static, str> {
    if qcode_kernel::providers::oauth2::is_oauth2_route(&row.route) {
        qcode_i18n::tr("gui.setup.editor.oauth2_type_other")
    } else {
        row.protocol.label()
    }
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
        // V3: 「添加模型/供应商」→ Provider 选择框（只选择）→ 配置页；入口
        // 放在列表顶部（原默认行下拉按钮的位置）。
        cards = cards.child(
            div().flex().flex_row().gap_2().child(
                Button::new("open-provider-picker")
                    .label(qcode_i18n::tr("gui.setup.card.add_provider"))
                    // 编辑页/加模型弹框打开时禁用：点击套餐等操作的重渲染会让
                    // mouseup 落到这个按钮上，误触发 open_provider_picker 把正在
                    // 编辑的 provider 关掉（switch_package → cx.notify() 重渲染）。
                    .when(
                        self.editing_provider.is_some() || self.adding_model_provider.is_some(),
                        |button| button.disabled(true),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_provider_picker(window, cx);
                    })),
            ),
        );
        if let Some(notice) = &self.saved_notice {
            cards = cards.child(
                div()
                    .text_size(sp(SECONDARY_SIZE))
                    .text_color(success_color)
                    .child(notice.to_string()),
            );
        }
        // V3: 只展示「已配置了模型」的 provider（`configured_models`：profile
        // 存在且 models 非空或 default_model 已设置）；没有配置模型的不展示。
        let filtered: Vec<&qcode_kernel::provider_registry::ConfigurableProvider> = directory
            .iter()
            .filter(|row| {
                qcode_kernel::provider_registry::configured_models(&row.route, &self.config)
            })
            .collect();
        if filtered.is_empty() {
            cards = cards.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(muted_color)
                    .child(qcode_i18n::tr("gui.setup.card.empty")),
            );
        }
        for row in filtered {
            let route = row.route.clone();
            // 显示名：套餐条目为「厂商名 (套餐名)」，其余为「产品名 (route)」。
            let display = row.label();
            // 默认 provider 前显示实心绿点；其余渲染同尺寸的透明占位
            // （无背景），保证卡片文字左侧对齐。
            let is_default = self.config.default_model.provider.as_deref() == Some(route.as_str());
            let expanded = self.expanded_routes.contains(&route);
            let mut head = div().flex().flex_row().items_center().gap_2();
            head = head.child(
                div()
                    .size(px(8.0))
                    .rounded_full()
                    .when(is_default, |this| this.bg(success_color)),
            );
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
                                    provider_type_label(row),
                                    credential_text(row.credential_state)
                                )),
                        ),
                )
                .child(div().flex_grow(1.0));
            // 已配置且可用（凭证齐全或无需密钥）的 provider，只要还不是
            // 默认，就在卡片上给一个「设为默认」快捷按钮。
            let usable = self.provider_usable(row);
            if row.configured && usable && !is_default {
                let route_for_default = route.clone();
                head = head.child(
                    Button::new(format!("default-provider-{route}"))
                        .label(qcode_i18n::tr("ui.action.set_default"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.make_default(&route_for_default, window, cx);
                        })),
                );
            }
            // 编辑在弹出对话框里进行（模型增删等动作也移入对话框）。
            let route_for_edit = route.clone();
            head = head.child(
                Button::new(format!("edit-provider-{route}"))
                    .label(qcode_i18n::tr("ui.action.edit"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_editor(&route_for_edit, true, window, cx);
                    })),
            );
            // 删除走同一个确认弹框（`render_delete_dialog`）。
            let route_for_delete = route.clone();
            head = head.child(
                Button::new(format!("delete-provider-{route}"))
                    .label(qcode_i18n::tr("ui.action.delete"))
                    .danger()
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.saved_notice = None;
                        this.editing_provider = None;
                        this.delete_target = Some(route_for_delete.clone());
                        cx.notify();
                    })),
            );
            // 展开/收起模型列表。
            let route_for_caret = route.clone();
            let head_caret = Button::new(format!("expand-provider-{route}"))
                .label(if expanded {
                    qcode_i18n::tr("gui.setup.card.collapse")
                } else {
                    qcode_i18n::tr("gui.setup.card.expand")
                })
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.toggle_provider_expanded(&route_for_caret, cx);
                }));
            head = head.child(head_caret);

            let mut card = div()
                .flex()
                .flex_col()
                .gap_2()
                .border_1()
                .border_color(border_color)
                .rounded(px(10.0))
                .p_3()
                .child(head);
            if expanded {
                let models =
                    qcode_kernel::provider_registry::effective_models(&route, &self.config);
                // 模型行列表：最多同时显示 4 行（约 26px/行 + gap_1 = 116px），
                // 超过 4 行则纵向滚动；「配置模型」按钮保持在滚动区外。
                let mut list = div()
                    .id(format!("expanded-models-{route}"))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when(models.len() > 4, |this| {
                        this.max_h(px(116.0)).overflow_y_scroll()
                    });
                for model in &models {
                    let label = match &model.name {
                        Some(name) if name != &model.id => format!("{} — {name}", model.id),
                        _ => model.id.clone(),
                    };
                    // 未知值统一写 `-`，与中文界面保持一致：这一行是仪器读数，
                    // 不是句子，翻译层不该替它决定「未知」怎么写。
                    let caps = qcode_i18n::trp(
                        "gui.setup.card.caps",
                        &[
                            model
                                .context_window
                                .map(|value| value.to_string())
                                .unwrap_or_else(|| "-".to_string()),
                            model
                                .max_tokens
                                .map(|value| value.to_string())
                                .unwrap_or_else(|| "-".to_string()),
                        ],
                    );
                    list = list.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .pl_6()
                            .py_1()
                            .border_t_1()
                            .border_color(border_color)
                            .child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .text_size(sp(CODE_SIZE))
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_size(sp(CAPTION_SIZE))
                                    .text_color(muted_color)
                                    .child(caps),
                            ),
                    );
                }
                if models.is_empty() {
                    list = list.child(
                        div()
                            .px_2()
                            .pt_2()
                            .border_t_1()
                            .border_color(border_color)
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(muted_color)
                            .child(qcode_i18n::tr("gui.setup.card.no_models")),
                    );
                }
                card = card.child(list);
                let route_manage = route.clone();
                card = card.child(
                    div().pt_1().child(
                        Button::new(format!("manage-models-{route}"))
                            .label(qcode_i18n::tr("gui.setup.card.configure_model"))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.open_add_model(&route_manage, cx);
                            })),
                    ),
                );
            }
            cards = cards.child(card);
        }
        cards.into_any_element()
    }

    /// The「添加模型」Provider 选择框（modal，只选择不配置）：顶部固定
    /// 「自定义提供方…」入口，下面列目录中的全部 Provider（catalog +
    /// 手写 route），点击某行即关闭并进入配置页。
    pub(crate) fn render_provider_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.wizard || !self.adding_provider {
            return div().into_any_element();
        }
        let theme = cx.theme();
        let (popover, popover_fg, border_color, muted_color, hovered) = (
            theme.popover,
            theme.popover_foreground,
            theme.border,
            theme.muted_foreground,
            theme.secondary,
        );
        let directory = qcode_kernel::provider_registry::ProviderRegistry::new(None)
            .list_configurable_providers(&self.config);
        // 凭证状态按 route 建索引：套餐分支（`picker_vendor`）也要查，而
        // `directory` 在下面会被 partition 移走。
        let by_route: std::collections::BTreeMap<String, _> = directory
            .iter()
            .map(|row| (row.route.clone(), row.credential_state))
            .collect();
        // 已配置（凭证可用）的排在前面，免得用户总得滚动才能看到；组内保持
        // 目录顺序（稳定分区，不是排序——排序会把用户自定义的手写 route 打乱）。
        let (mut usable, mut missing) = directory.into_iter().partition::<Vec<_>, _>(|row| {
            !matches!(
                row.credential_state,
                qcode_kernel::provider_registry::CredentialState::Missing
            )
        });
        usable.append(&mut missing);
        let mut rows = div().flex().flex_col().gap_1();
        rows = rows.child(
            div()
                .mb_1()
                .pb_2()
                .border_b_1()
                .border_color(border_color)
                .child(
                    div()
                        .id("picker-custom-provider")
                        .flex()
                        .items_center()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|this| this.bg(hovered))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.pick_custom(window, cx);
                        }))
                        .child(qcode_i18n::tr("gui.setup.card.custom_provider")),
                ),
        );
        // 包型厂商在列表里以厂商行出现，套餐在配置详情页里选；套餐行不在此
        // 平铺，保证列表干净（无括号、无套餐选择）。
        for row in &usable {
            // 本地 Ollama 不在「添加模型」选择框里提供：模型需自行配置，
            // 没有「一键添加」的预设。
            if row.route == "ollama" {
                continue;
            }
            if row.package_label.is_some() {
                continue;
            }
            let route = row.route.clone();
            // 选择框只显示产品名，不带 (route) 括号（route 在副文本里）。
            let display = row.display_name.clone();
            let dot = match row.credential_state {
                qcode_kernel::provider_registry::CredentialState::Configured => {
                    Some(div().size(px(8.0)).rounded_full().bg(theme.success))
                }
                qcode_kernel::provider_registry::CredentialState::Missing => Some(
                    div()
                        .size(px(8.0))
                        .rounded_full()
                        .border_1()
                        .border_color(muted_color),
                ),
                qcode_kernel::provider_registry::CredentialState::Keyless => None,
            };
            let route_pick = route.clone();
            let mut entry = div()
                .id(format!("picker-provider-{route}"))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(move |this| this.bg(hovered))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.pick_provider(&route_pick, window, cx);
                }));
            if let Some(dot) = dot {
                entry = entry.child(dot);
            }
            entry = entry
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
                                    provider_type_label(row),
                                    credential_text(row.credential_state)
                                )),
                        ),
                )
                .child(div().flex_grow(1.0));
            rows = rows.child(entry);
        }
        // 包型厂商行：点击进入配置详情页，套餐在详情页里选择。
        for group in qcode_kernel::config::package_groups() {
            let Some(first) = group.packages.first() else {
                continue;
            };
            let route = first.route.to_string();
            let vendor_label = group.display_name;
            let state = by_route.get(&route).copied();
            let dot = match state {
                Some(qcode_kernel::provider_registry::CredentialState::Configured) => {
                    Some(div().size(px(8.0)).rounded_full().bg(theme.success))
                }
                Some(qcode_kernel::provider_registry::CredentialState::Missing) => Some(
                    div()
                        .size(px(8.0))
                        .rounded_full()
                        .border_1()
                        .border_color(muted_color),
                ),
                _ => None,
            };
            let route_pick = route.clone();
            let mut entry = div()
                .id(format!("picker-vendor-{route}"))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(move |this| this.bg(hovered))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.pick_provider(&route_pick, window, cx);
                }));
            if let Some(dot) = dot {
                entry = entry.child(dot);
            }
            entry = entry
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().child(vendor_label))
                        .child(
                            div()
                                .text_size(sp(CAPTION_SIZE))
                                .text_color(muted_color)
                                .child(format!(
                                    "{} · {}",
                                    qcode_kernel::config::Protocol::OpenAi.label(),
                                    credential_text(state.unwrap_or(
                                        qcode_kernel::provider_registry::CredentialState::Missing
                                    ))
                                )),
                        ),
                )
                .child(div().flex_grow(1.0));
            rows = rows.child(entry);
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
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(popover)
                    .border_1()
                    .border_color(border_color)
                    .p_4()
                    .text_color(popover_fg)
                    .child(div().child(qcode_i18n::tr("gui.setup.editor.choose_provider")))
                    .child(
                        div()
                            .id("provider-picker-scroll")
                            .flex()
                            .flex_col()
                            .gap_1()
                            .max_h(px(360.0))
                            .overflow_y_scroll()
                            .child(rows),
                    )
                    .child(
                        div().flex().flex_row().justify_end().child(
                            Button::new("cancel-provider-picker")
                                .label(qcode_i18n::tr("ui.action.cancel"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.close_provider_picker(cx);
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// 「为某 Provider 添加模型」的纯添加弹框（modal）：只含模型目录
    /// 编辑器（现有模型行 + 添加/删除 + 获取可用模型），不含 API 密钥、
    /// 更多配置与删除 Provider。
    pub(crate) fn render_add_model_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.wizard {
            return div().into_any_element();
        }
        let Some(route) = &self.adding_model_provider else {
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
                    .w(px(520.0))
                    // 与 Provider 编辑弹窗一致：整体封顶到窗口高度的 90%，
                    // 模型列表超出时由 flex_1 滚动区吸收，标题与按钮固定。
                    .max_h(relative(0.9))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(popover)
                    .border_1()
                    .border_color(border_color)
                    .p_4()
                    .text_color(popover_fg)
                    .child(div().child(qcode_i18n::trp(
                        "gui.setup.editor.add_model_title",
                        &[route],
                    )))
                    .child(
                        div()
                            .id("add-model-dialog-scroll")
                            .flex_1()
                            .min_h(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_3()
                            .overflow_y_scroll()
                            .child(self.render_models_editor(row, route, cx)),
                    )
                    .child(match &self.provider_error {
                        Some(error) => div()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(danger_color)
                            .child(error.to_string())
                            .into_any_element(),
                        None => div().into_any_element(),
                    })
                    .child(
                        div().flex().flex_row().justify_end().child(
                            Button::new("close-add-model-dialog")
                                .label(qcode_i18n::tr("ui.action.save"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    // 弹框内的模型行编辑只在点「保存」时
                                    // 写盘。直接用 flush 的 Result 判断成败：
                                    // 成功才关闭，失败保持弹框打开并显示原因。
                                    match this.flush_auto_save(AutoSaveKind::Tables, cx) {
                                        Ok(()) => {
                                            this.provider_error = None;
                                            // 延迟到当前事件处理结束再关闭弹窗：
                                            // 同一事件内 close 的 notify 若触发
                                            // 渲染会读到尚未生效的状态（GPUI
                                            // 渲染缓存），跨到下一帧再关。
                                            cx.defer_in(window, |view, _window, cx| {
                                                view.close_add_model(cx);
                                            });
                                        }
                                        Err(error) => {
                                            // 保存失败：保持弹框打开并把原因
                                            // 显示在弹框内。
                                            this.provider_error = Some(error);
                                            cx.notify();
                                        }
                                    }
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// 太极统一认证的登录状态区（需求 4c：无 API 密钥/钥匙串，改成登录态）：
    /// 读 `tj_auth.json` 显示 未登录 / 已登录（用户名），提供「登录」按钮。
    fn render_tj_login(&self, route: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted_color, success_color) = (theme.muted_foreground, theme.success);
        let store_path = self.tj_store_path();
        let signed_in = qcode_providers::TjTokenStore::new(store_path)
            .load()
            .ok()
            .filter(|tokens| !tokens.access_token.is_empty())
            .map(|tokens| {
                tokens.username().unwrap_or_else(|| {
                    qcode_i18n::tr("gui.setup.editor.tj_account_fallback").to_string()
                })
            });
        let (status, color) = match &signed_in {
            Some(username) => (
                qcode_i18n::trp("gui.setup.editor.tj_status_signed_in", &[username.as_str()]),
                success_color,
            ),
            None => (
                qcode_i18n::tr("gui.setup.editor.tj_status_signed_out").to_string(),
                muted_color,
            ),
        };
        let route_for_login = route.to_string();
        let mut section = div().flex().flex_col().gap_1();
        section = section.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().size(px(8.0)).rounded_full().bg(color))
                .child(div().text_size(sp(CAPTION_SIZE)).child(status)),
        );
        section = section.child(
            Button::new(format!("tj-login-{route}"))
                .label(qcode_i18n::tr("gui.setup.editor.tj_login"))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    // 登录：打开 Keycloak 登录页，loopback 回调自动收回 code
                    // （见 kernel tj `open_browser_and_wait`），无需手动粘贴。
                    this.login_tj(&route_for_login, cx);
                })),
        );
        // 按钮下方的登录反馈：成功显示绿色「登录成功」，进行中显示灰色提示。
        let signed_in = qcode_i18n::tr("gui.setup.editor.tj_signed_in").to_string();
        let pending = qcode_i18n::tr("gui.setup.editor.tj_pending").to_string();
        match self.provider_notice.as_deref() {
            Some(notice) if notice == signed_in.as_str() => {
                section = section.child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(success_color)
                        .child(notice.to_string()),
                );
            }
            Some(notice) if notice == pending.as_str() => {
                section = section.child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(muted_color)
                        .child(notice.to_string()),
                );
            }
            _ => {}
        }
        section.into_any_element()
    }

    /// 触发太极统一认证的授权码流程（设置页「登录」按钮）。reqwest 需要 tokio
    /// reactor（GPUI executor 不是），所以登录在 worker 线程跑，结果经 channel
    /// 回报、前台轮询更新提示。
    fn login_tj(&mut self, _route: &str, cx: &mut Context<Self>) {
        let Some(prompt) = qcode_kernel::providers::tj::global_auth_prompt() else {
            self.provider_error = Some(qcode_i18n::tr("gui.setup.editor.tj_no_prompt").to_string());
            cx.notify();
            return;
        };
        let store_path = self.tj_store_path();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = qcode_kernel::providers::tj::login(&store_path, prompt);
            let _ = tx.send(result);
        });
        self.tj_login_rx = Some(rx);
        self.provider_error = None;
        self.provider_notice = Some(qcode_i18n::tr("gui.setup.editor.tj_pending").to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let done = this.update(cx, |view, cx| {
                    let received = view.tj_login_rx.as_ref().map(|rx| rx.try_recv());
                    match received {
                        Some(Ok(Ok(()))) => {
                            view.tj_login_rx = None;
                            view.provider_notice =
                                Some(qcode_i18n::tr("gui.setup.editor.tj_signed_in").to_string());
                            view.provider_error = None;
                            cx.notify();
                            true
                        }
                        Some(Ok(Err(error))) => {
                            view.tj_login_rx = None;
                            view.provider_notice = None;
                            view.provider_error = Some(error);
                            cx.notify();
                            true
                        }
                        Some(Err(std::sync::mpsc::TryRecvError::Empty)) => false,
                        Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                            view.tj_login_rx = None;
                            view.provider_error =
                                Some(qcode_i18n::tr("gui.setup.editor.tj_interrupted").to_string());
                            cx.notify();
                            true
                        }
                        None => true,
                    }
                });
                match done {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        })
        .detach();
    }

    /// `tj_auth.json` 的位置：与 config.toml 同目录。
    pub(crate) fn tj_store_path(&self) -> std::path::PathBuf {
        self.path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("tj_auth.json")
    }

    /// `oauth2_auth.json` 的位置：与 config.toml 同目录。
    pub(crate) fn oauth2_store_path(&self) -> std::path::PathBuf {
        self.path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("oauth2_auth.json")
    }

    /// 从回调地址里取出端口（`http://127.0.0.1:43782/oauth2/callback` → 43782）。
    ///
    /// host 与路径固定，只有端口参与登录监听；解析失败（格式异常 / 无端口）
    /// 返回 `None`，调用方落回默认端口 [`LOOPBACK_PORT`]。
    fn redirect_uri_port(uri: &str) -> Option<u16> {
        uri.split_once("://")?
            .1
            .split_once('/')?
            .0
            .rsplit_once(':')?
            .1
            .parse()
            .ok()
    }

    /// 初始化可配置 OAuth2 的草稿（打开编辑框时）：从 config 的
    /// `providers.<route>.oauth2` 读值建输入框；没有配置时用默认值
    /// （PKCE + 默认 loopback 回调）。与太极（硬编码）不同，
    /// 这些端点/客户端标识是用户配置，必须在编辑框里可改。
    fn init_oauth2_draft(&mut self, route: &str, window: &mut Window, cx: &mut Context<Self>) {
        let oauth2 = self
            .config
            .providers
            .get(route)
            .and_then(|profile| profile.oauth2.clone());
        self.oauth2_mode = oauth2
            .as_ref()
            .map(|config| config.mode.as_str().to_string())
            // 默认 PKCE（public，无需 Client Secret）。
            .unwrap_or_else(|| "pkce".to_string());
        let label = |key: &'static str| qcode_i18n::tr(key).to_string();
        self.oauth2_client_id = Some(input(
            window,
            cx,
            oauth2
                .as_ref()
                .map(|c| c.client_id.clone())
                .unwrap_or_default(),
            &label("gui.setup.editor.oauth2_client_id_label"),
        ));
        // client_secret 明文存于 config（已确认的决策），打开时 masked 回显。
        self.oauth2_client_secret = Some(secret_input(
            window,
            cx,
            &label("gui.setup.editor.oauth2_client_secret_label"),
        ));
        if let Some(secret) = &self.oauth2_client_secret
            && let Some(value) = oauth2.as_ref().and_then(|c| c.client_secret.clone())
        {
            secret.update(cx, |state, cx| {
                state.set_value(value, window, cx);
            });
        }
        // 密钥来源：只在 GUI 编辑明文（ADR-0107 的 env/command 两个非明文来源
        // 不进设置页，配置走 config.toml，见 ADR-0107）。
        self.oauth2_auth_url = Some(input(
            window,
            cx,
            oauth2
                .as_ref()
                .map(|c| c.auth_url.clone())
                .unwrap_or_default(),
            &label("gui.setup.editor.oauth2_auth_url_label"),
        ));
        self.oauth2_token_url = Some(input(
            window,
            cx,
            oauth2
                .as_ref()
                .map(|c| c.token_url.clone())
                .unwrap_or_default(),
            &label("gui.setup.editor.oauth2_token_url_label"),
        ));
        self.oauth2_scope = Some(input(
            window,
            cx,
            oauth2
                .as_ref()
                .and_then(|c| c.scope.clone())
                .unwrap_or_default(),
            &label("gui.setup.editor.oauth2_scope_label"),
        ));
        // 回调地址：host 与路径固定（loopback 监听永远绑 127.0.0.1，见 kernel
        // loopback 模块），只开放端口编辑——改 host 的配置本就不会生效。
        // 输入即时校验：只接受能解析成 u16 的值，超端口上限（65535）或非数字
        // 的输入会被直接拒绝。
        let redirect_port = oauth2
            .as_ref()
            .and_then(|c| c.redirect_uri.as_deref())
            .and_then(Self::redirect_uri_port)
            .unwrap_or(qcode_kernel::providers::loopback::LOOPBACK_PORT);
        let port_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(label("gui.setup.editor.oauth2_redirect_uri_label"))
                .validate(|text, _| text.parse::<u16>().is_ok())
        });
        port_input.update(cx, |state, cx| {
            state.set_value(redirect_port.to_string(), window, cx);
        });
        self.oauth2_redirect_port = Some(port_input);
    }

    /// 从草稿实体读值构造 `OAuth2AuthSettings`（登录用）。
    ///
    /// 失败时返回**可显示的原因**，而不是一个 None：密钥配成「从环境变量取」却
    /// 没导出那个变量时，界面上必须点名缺的是哪个变量——只说「配置不完整」会把
    /// 人送去反复检查端点，而那些是对的。校验顺序：必填项 → 密钥来源。
    fn oauth2_draft_settings(
        &self,
        cx: &Context<Self>,
    ) -> Result<qcode_providers::OAuth2AuthSettings, String> {
        let client_id = self
            .oauth2_client_id
            .as_ref()
            .map(|entity| entity.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let auth_url = self
            .oauth2_auth_url
            .as_ref()
            .map(|entity| entity.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let token_url = self
            .oauth2_token_url
            .as_ref()
            .map(|entity| entity.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if client_id.is_empty() || auth_url.is_empty() || token_url.is_empty() {
            return Err(qcode_i18n::tr("gui.setup.editor.oauth2_incomplete").to_string());
        }
        let mode = if self.oauth2_mode == "pkce" {
            qcode_providers::OAuth2Mode::Pkce
        } else {
            qcode_providers::OAuth2Mode::AuthorizationCode
        };
        // 密钥按 env > command > 明文的优先级解析，与 kernel 是同一个函数
        // （ADR-0107 补记）：设置页的「登录」按钮必须与运行时拿到同一个密钥，
        // 否则会出现「设置页能登录、跑任务却 401」。
        let draft = self.oauth2_draft_config(cx);
        let client_secret = qcode_kernel::config::oauth2_client_secret(
            qcode_kernel::providers::oauth2::OAUTH2_ROUTE,
            &draft,
            &|key| std::env::var(key).ok(),
        )
        .map_err(|error| error.to_string())?;
        let scope = self
            .oauth2_scope
            .as_ref()
            .map(|entity| entity.read(cx).value().trim().to_string())
            .filter(|scope| !scope.is_empty());
        // 回调地址只暴露端口（host 与路径固定），拼回完整 URL；端口非法/留空
        // 落回默认 loopback 地址。
        let redirect_uri = self
            .oauth2_redirect_port
            .as_ref()
            .map(|entity| entity.read(cx).value().trim().to_string())
            .and_then(|port| port.parse::<u16>().ok())
            .map(|port| format!("http://127.0.0.1:{port}/oauth2/callback"))
            .unwrap_or_else(qcode_kernel::providers::oauth2::default_redirect_uri);
        Ok(qcode_providers::OAuth2AuthSettings {
            mode,
            client_id,
            client_secret,
            auth_url,
            token_url,
            scope,
            redirect_uri,
        })
    }

    /// 从草稿实体读值构造 `OAuth2AuthConfig`（保存时写进 config 的
    /// `providers.<route>.oauth2`）。字段缺省/为空也返回 Some（空串 + 默认
    /// 模式），保证 route 保持 oauth2 身份——必填校验在登录/探测阶段做。
    pub(crate) fn oauth2_draft_config(
        &self,
        cx: &Context<Self>,
    ) -> qcode_kernel::config::OAuth2AuthConfig {
        let read = |entity: Option<&Entity<InputState>>| {
            entity
                .map(|entity| entity.read(cx).value().trim().to_string())
                .unwrap_or_default()
        };
        let non_empty = |value: String| {
            if value.is_empty() { None } else { Some(value) }
        };
        qcode_kernel::config::OAuth2AuthConfig {
            mode: if self.oauth2_mode == "pkce" {
                qcode_providers::OAuth2Mode::Pkce
            } else {
                qcode_providers::OAuth2Mode::AuthorizationCode
            },
            client_id: read(self.oauth2_client_id.as_ref()),
            client_secret: non_empty(read(self.oauth2_client_secret.as_ref())),
            // 密钥的 env/command 两个非明文来源不进设置页（ADR-0107）：GUI 保存
            // 只写明文这一份；要用前两者得手改 config.toml。
            auth_url: read(self.oauth2_auth_url.as_ref()),
            token_url: read(self.oauth2_token_url.as_ref()),
            scope: non_empty(read(self.oauth2_scope.as_ref())),
            // 回调地址：host 与路径固定，只存用户编辑的端口（拼回完整 URL）；
            // 端口非法/留空 → None，运行时落回默认 loopback 地址。
            redirect_uri: self
                .oauth2_redirect_port
                .as_ref()
                .map(|entity| entity.read(cx).value().trim().to_string())
                .and_then(|port| port.parse::<u16>().ok())
                .map(|port| format!("http://127.0.0.1:{port}/oauth2/callback")),
            // env/command 两个非明文来源不在表单里，保存时置空（见上方注释）。
            ..Default::default()
        }
    }

    /// 可配置 OAuth2 统一认证的配置区 + 登录状态区（设置页编辑弹框内）。
    ///
    /// 布局：端点（授权/令牌地址、scope、回调）在前——两种模式都要填；登录模式
    /// 按钮居中；客户端标识按模式增减：授权码（confidential）展示 Client ID +
    /// Client Secret（换 token 带 client_secret），PKCE（public，默认）只展示
    /// Client ID（授权 URL 带 S256 code_challenge）。端点/客户端标识是用户配置
    /// （写入 `providers.<route>.oauth2`），与太极的硬编码不同。
    fn render_oauth2_config(&self, route: &str, cx: &mut Context<Self>) -> AnyElement {
        let label = |key: &'static str| qcode_i18n::tr(key).to_string();
        let mut section = div().flex().flex_col().gap_2();
        let field_row = |key: &'static str, entity: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(sp(CAPTION_SIZE)).child(label(key)))
                .child(Input::new(entity).aria_label(label(key)))
        };
        // 端点（共用，两种模式都要填）在最前。
        for (key, entity) in [
            (
                "gui.setup.editor.oauth2_auth_url_label",
                &self.oauth2_auth_url,
            ),
            (
                "gui.setup.editor.oauth2_token_url_label",
                &self.oauth2_token_url,
            ),
            ("gui.setup.editor.oauth2_scope_label", &self.oauth2_scope),
        ] {
            if let Some(entity) = entity {
                section = section.child(field_row(key, entity));
            }
        }
        // 回调地址：host 与路径固定（loopback 监听永远绑 127.0.0.1），只开放端口。
        if let Some(entity) = &self.oauth2_redirect_port {
            section = section.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .child(qcode_i18n::tr("gui.setup.editor.oauth2_redirect_uri_label")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(sp(SECONDARY_SIZE))
                                    .child("http://127.0.0.1:"),
                            )
                            .child(
                                Input::new(entity)
                                    .w(px(64.0))
                                    .with_size(gpui_kit::component::Size::Small)
                                    .aria_label(qcode_i18n::tr(
                                        "gui.setup.editor.oauth2_redirect_uri_label",
                                    )),
                            )
                            .child(
                                div()
                                    .text_size(sp(SECONDARY_SIZE))
                                    .child("/oauth2/callback"),
                            ),
                    ),
            );
        }
        // 登录模式：PKCE（默认）/ 授权码 双按钮。
        let mut mode_row = div().flex().flex_row().gap_1();
        for (value, label_key) in [
            ("pkce", "gui.setup.editor.oauth2_mode_pkce"),
            (
                "authorization_code",
                "gui.setup.editor.oauth2_mode_auth_code",
            ),
        ] {
            let marker = if self.oauth2_mode == value {
                "●"
            } else {
                "○"
            };
            let value = value.to_string();
            let label = qcode_i18n::tr(label_key).to_string();
            mode_row = mode_row.child(
                Button::new(format!("oauth2-mode-{value}-{route}"))
                    .label(format!("{marker} {label}"))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.oauth2_mode = value.clone();
                        cx.notify();
                    })),
            );
        }
        section = section.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .child(qcode_i18n::tr("gui.setup.editor.oauth2_mode_label")),
                )
                .child(mode_row),
        );
        // 客户端标识：Client ID 始终显示；Client Secret 仅授权码模式显示。
        if let Some(entity) = &self.oauth2_client_id {
            section = section.child(field_row("gui.setup.editor.oauth2_client_id_label", entity));
        }
        if self.oauth2_mode == "authorization_code"
            && let Some(entity) = &self.oauth2_client_secret
        {
            section = section.child(field_row(
                "gui.setup.editor.oauth2_client_secret_label",
                entity,
            ));
        }
        // 登录状态区。
        section = section.child(self.render_oauth2_login(route, cx));
        section.into_any_element()
    }

    /// 可配置 OAuth2 统一认证的登录状态区：读 `oauth2_auth.json` 显示
    /// 未登录 / 已登录（用户名），提供「登录」按钮。
    fn render_oauth2_login(&self, route: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted_color, success_color) = (theme.muted_foreground, theme.success);
        let store_path = self.oauth2_store_path();
        let signed_in = qcode_providers::TjTokenStore::new(store_path)
            .load()
            .ok()
            .filter(|tokens| !tokens.access_token.is_empty())
            .map(|tokens| {
                tokens.username().unwrap_or_else(|| {
                    qcode_i18n::tr("gui.setup.editor.oauth2_account_fallback").to_string()
                })
            });
        let (status, color) = match &signed_in {
            Some(username) => (
                qcode_i18n::trp(
                    "gui.setup.editor.oauth2_status_signed_in",
                    &[username.as_str()],
                ),
                success_color,
            ),
            None => (
                qcode_i18n::tr("gui.setup.editor.oauth2_status_signed_out").to_string(),
                muted_color,
            ),
        };
        let route_for_login = route.to_string();
        let mut section = div().flex().flex_col().gap_1();
        section = section.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().size(px(8.0)).rounded_full().bg(color))
                .child(div().text_size(sp(CAPTION_SIZE)).child(status)),
        );
        section = section.child(
            Button::new(format!("oauth2-login-{route}"))
                .label(qcode_i18n::tr("gui.setup.editor.oauth2_login"))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.login_oauth2(&route_for_login, cx);
                })),
        );
        // 按钮下方的登录反馈：成功显示绿色「登录成功」，进行中显示灰色提示。
        let signed_in = qcode_i18n::tr("gui.setup.editor.oauth2_signed_in").to_string();
        let pending = qcode_i18n::tr("gui.setup.editor.oauth2_pending").to_string();
        match self.provider_notice.as_deref() {
            Some(notice) if notice == signed_in.as_str() => {
                section = section.child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(success_color)
                        .child(notice.to_string()),
                );
            }
            Some(notice) if notice == pending.as_str() => {
                section = section.child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(muted_color)
                        .child(notice.to_string()),
                );
            }
            _ => {}
        }
        section.into_any_element()
    }

    /// 触发可配置 OAuth2 统一认证的登录流程（设置页「登录」按钮）。从草稿
    /// 构造 `OAuth2AuthSettings` 显式传给 kernel（kernel 读不到 GUI 草稿），
    /// worker 线程跑登录、结果经 channel 回报、前台轮询更新提示。
    fn login_oauth2(&mut self, _route: &str, cx: &mut Context<Self>) {
        let Some(prompt) = qcode_kernel::providers::oauth2::global_oauth2_prompt() else {
            self.provider_error =
                Some(qcode_i18n::tr("gui.setup.editor.oauth2_no_prompt").to_string());
            cx.notify();
            return;
        };
        let settings = match self.oauth2_draft_settings(cx) {
            Ok(settings) => settings,
            // 原因已经是可以直接显示的话（缺哪个变量、缺哪个必填项）。
            Err(reason) => {
                self.provider_error = Some(reason);
                cx.notify();
                return;
            }
        };
        let store_path = self.oauth2_store_path();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = qcode_kernel::providers::oauth2::login(&store_path, settings, prompt);
            let _ = tx.send(result);
        });
        self.oauth2_login_rx = Some(rx);
        self.provider_error = None;
        self.provider_notice = Some(qcode_i18n::tr("gui.setup.editor.oauth2_pending").to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let done = this.update(cx, |view, cx| {
                    let received = view.oauth2_login_rx.as_ref().map(|rx| rx.try_recv());
                    match received {
                        Some(Ok(Ok(()))) => {
                            view.oauth2_login_rx = None;
                            view.provider_notice = Some(
                                qcode_i18n::tr("gui.setup.editor.oauth2_signed_in").to_string(),
                            );
                            view.provider_error = None;
                            cx.notify();
                            true
                        }
                        Some(Ok(Err(error))) => {
                            view.oauth2_login_rx = None;
                            view.provider_notice = None;
                            view.provider_error = Some(error);
                            cx.notify();
                            true
                        }
                        Some(Err(std::sync::mpsc::TryRecvError::Empty)) => false,
                        Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                            view.oauth2_login_rx = None;
                            view.provider_error = Some(
                                qcode_i18n::tr("gui.setup.editor.oauth2_interrupted").to_string(),
                            );
                            cx.notify();
                            true
                        }
                        None => true,
                    }
                });
                match done {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        })
        .detach();
    }

    /// 切换详情页的套餐：确保表格里有目标套餐的行（没有就按预设建行），并把
    /// 编辑对象切到目标套餐 route。不重开编辑页、不做任何写盘——写盘由「保存」
    /// 按钮统一触发。
    fn switch_package(
        &mut self,
        target: &str,
        _row_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 编辑已有 Provider 时套餐锁定：按钮已禁用，这里再防一道，防止
        // 事件误路由（如重渲染导致 mouseup 落错按钮）偷偷切套餐。
        if self.editing_package_locked {
            return;
        }
        let Some((group, _)) = qcode_kernel::config::package_entry(target) else {
            return;
        };
        // 套餐单选：切到目标套餐时，把同组其他套餐行移出表格。否则保存时
        // `apply_tables` 遍历所有行，残留的旧套餐行（需要密钥但没模型）会
        // 触发「提供方没有模型」校验，整个保存被拒。
        let group_routes = group
            .packages
            .iter()
            .map(|pkg| pkg.route)
            .collect::<Vec<_>>();
        if let Some(table) = self.tables.get_mut("providers") {
            table.rows.retain(|row| {
                let name = cell(row, "name", cx).trim().to_string();
                !group_routes.contains(&name.as_str()) || name == target
            });
        }
        let has_row = self
            .tables
            .get("providers")
            .map(|table| {
                table
                    .rows
                    .iter()
                    .any(|row| cell(row, "name", cx).trim() == target)
            })
            .unwrap_or(false);
        if !has_row {
            self.add_provider_preset(target, window, cx);
        }
        self.editing_provider = Some(target.to_string());
        self.provider_error = None;
        self.provider_notice = None;
        cx.notify();
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
            .map(|row| row.label())
            .unwrap_or_else(|| route.clone());
        let configured = entry.map(|row| row.configured).unwrap_or(false);
        let usable = entry.map(|row| self.provider_usable(row)).unwrap_or(false);
        let is_default = self.config.default_model.provider.as_deref() == Some(route.as_str());
        let key = "providers";
        let row_id = row.id;
        let columns = forms::TABLES
            .iter()
            .find(|spec| spec.path.join(".") == key)
            .map(|spec| spec.columns)
            .unwrap_or(&[]);

        let mut body = div().flex().flex_col().gap_3();
        // 套餐选择：有套餐的 Provider 在 API 密钥上面显示套餐下拉（无套餐的
        // Provider 不展示）。切套餐即切换 route（一个套餐 = 一个 route）。
        // 编辑已有 Provider 时套餐锁定（`editing_package_locked`），不可切换。
        if let Some((group, _)) = qcode_kernel::config::package_entry(route.as_str()) {
            let row_id = row.id;
            let package_locked = self.editing_package_locked;
            let mut select = div().flex().flex_row().gap_1();
            for candidate in group.packages {
                let marker = if candidate.route == route {
                    "●"
                } else {
                    "○"
                };
                let target = candidate.route.to_string();
                let label = candidate.label.to_string();
                select = select.child(
                    Button::new(format!("table-providers-{row_id}-package-{target}"))
                        .label(format!("{marker} {label}"))
                        .when(package_locked, |button| button.disabled(true))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.switch_package(&target, row_id, window, cx);
                        })),
                );
            }
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .child(qcode_i18n::tr("gui.setup.editor.package")),
                    )
                    .child(select),
            );
        }
        // 第一条是 API 密钥：只有它需要在添加提供方时填写，其余默认都是
        // 固定值，折叠在展开按钮后面。太极统一认证没有钥匙串逻辑（需求 4c），
        // 这里换成登录状态区。可配置 OAuth2 统一认证没有 API 密钥，换成
        // 「配置区 + 登录状态区」（模式/端点/客户端标识由用户编辑）。
        if qcode_kernel::providers::oauth2::is_oauth2_route(route) {
            body = body.child(self.render_oauth2_config(route, cx));
        } else if qcode_kernel::providers::tj::is_tj_route(route) {
            body = body.child(self.render_tj_login(route, cx));
        } else if let Some(secret) = row.cells.get("__key") {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .child(qcode_i18n::tr("gui.setup.editor.api_key_hint")),
                    )
                    .child(Input::new(secret).aria_label(qcode_i18n::tr("ui.field.api_key")))
                    .child(
                        Button::new(format!("store-key-{key}-{row_id}"))
                            .label(qcode_i18n::tr("gui.setup.editor.store_keychain"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.store_provider_key(row_id, window, cx);
                            })),
                    ),
            );
        }
        // 模型配置固定显示在 API 密钥下面，不放进「展开更多配置」。
        body = body.child(self.render_models_editor(row, route, cx));
        let expanded = self.editor_expanded;
        body = body.child(
            Button::new("toggle-provider-config")
                .label(if expanded {
                    qcode_i18n::tr("gui.setup.editor.collapse_config")
                } else {
                    qcode_i18n::tr("gui.setup.editor.expand_config")
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
        // 太极 / OAuth2 路由的成功/进行中提示在登录按钮下方单独渲染（绿色），
        // 这里不重复。
        if !qcode_kernel::providers::tj::is_tj_route(route)
            && !qcode_kernel::providers::oauth2::is_oauth2_route(route)
            && let Some(notice) = &self.provider_notice
        {
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
                // 太极 / OAuth2 统一认证只有 openai 协议：隐藏协议选择，避免
                // 误切协议导致请求失败。
                if column.key == "protocol"
                    && (qcode_kernel::providers::tj::is_tj_route(route)
                        || qcode_kernel::providers::oauth2::is_oauth2_route(route))
                {
                    continue;
                }
                if let Some(field) = self.render_provider_field(row, column, cx) {
                    fields = fields.child(field);
                }
            }
            body = body.child(fields);
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
                        .label(qcode_i18n::tr("ui.action.set_default"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.make_default(&route_for_default, window, cx);
                        })),
                );
            }
            let route_for_delete = route.clone();
            footer = footer.child(
                Button::new(format!("dialog-delete-{route}"))
                    .label(qcode_i18n::tr("ui.action.delete"))
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
                    .label(qcode_i18n::tr("ui.action.cancel"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.close_editor(false);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("save-provider-edit")
                    .label(qcode_i18n::tr("ui.action.save"))
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
                        // 套餐锁定校验：编辑已有 Provider 时不允许更换套餐（route
                        // 不变）。按钮已禁用、`switch_package` 已防呆，这里在保存
                        // 时再兜底校验一次——若表格里该行被改名/换成别的套餐
                        // route，直接拒绝保存，而不是静默写进 config。
                        if this.editing_package_locked {
                            let locked_route = this.editing_provider.clone().unwrap_or_default();
                            let row_intact = this
                                .tables
                                .get("providers")
                                .map(|table| {
                                    table
                                        .rows
                                        .iter()
                                        .any(|row| cell(row, "name", cx).trim() == locked_route)
                                })
                                .unwrap_or(false);
                            if !row_intact {
                                this.provider_error = Some(
                                    qcode_i18n::tr("gui.setup.save_error.package_locked")
                                        .to_string(),
                                );
                                cx.notify();
                                return;
                            }
                        }
                        // 「保存」= 真正落盘：picker 选中的预设行此时才写进
                        // config、刷进列表。flush 失败（表单校验不过）时不
                        // 关对话框、不声称已保存。
                        match this.flush_auto_save(AutoSaveKind::Tables, cx) {
                            Ok(()) => {
                                this.close_editor(true);
                                this.saved_notice = Some(qcode_i18n::trp(
                                    "gui.setup.editor.saved",
                                    &[&route_close],
                                ));
                            }
                            Err(error) => {
                                this.provider_error = Some(error);
                            }
                        }
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
                    // 弹窗整体封顶到窗口高度的 90%：展开「更多配置」后内容变长时，
                    // 弹窗不再无限撑高超出设置窗口，而是由下面 flex_1 的滚动区
                    // 吸收剩余空间、纵向滚动；标题与底部按钮始终可见。
                    .max_h(relative(0.9))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(popover)
                    .border_1()
                    .border_color(border_color)
                    .p_4()
                    .text_color(popover_fg)
                    .child(
                        // 标题行：Provider 名字后面跟一个灰色小字的安全级别标识。
                        // 级别是内生字段（catalog/套餐表硬编码），不是配置项，
                        // 所以只读展示、没有编辑入口。
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                div().child(qcode_i18n::trp(
                                    "gui.setup.editor.edit_title",
                                    &[display],
                                )),
                            )
                            .child(
                                div()
                                    .text_size(sp(CAPTION_SIZE))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(qcode_kernel::config::security_level(route).label()),
                            ),
                    )
                    .child(
                        div()
                            .id("provider-dialog-scroll")
                            .flex_1()
                            .min_h(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_3()
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
                            // 编辑弹框改为「点击保存才保存」：只改行 state，
                            // 不写盘；写盘由对话框「保存」按钮统一触发。
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
                                    // 编辑弹框改为「点击保存才保存」。
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
                    .aria_label(qcode_i18n::tr(column.label))
                    .into_any_element()
            }
            _ => {
                let input = row.cells.get(column.key)?;
                let control = Input::new(input)
                    .aria_label(qcode_i18n::tr(column.label))
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
                            .child(qcode_i18n::trp(
                                "gui.setup.editor.global_override",
                                &[global],
                            )),
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
                .child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .child(qcode_i18n::tr(column.label)),
                )
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
            qcode_i18n::tr("gui.setup.catalog.custom")
        } else {
            qcode_i18n::tr("gui.setup.catalog.adapter_default")
        };
        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().child(qcode_i18n::tr("gui.setup.catalog.title")))
            .child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(muted_color)
                    .child(badge.clone()),
            )
            .child(div().flex_grow(1.0));
        if overridden {
            let row_id = row.id;
            header = header.child(
                Button::new(format!("reset-models-{row_id}"))
                    .label(qcode_i18n::tr("gui.setup.catalog.restore"))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.reset_models(row_id, cx);
                    })),
            );
        }
        let row_id = row.id;
        let route_probe = route.to_string();
        header = header.child(
            Button::new(format!("probe-models-{row_id}"))
                .label(qcode_i18n::tr("gui.setup.catalog.fetch"))
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
            // 模型行列表：最多同时显示 4 行（32px/行 + gap_1 = 140px），超过
            // 4 行则纵向滚动；「添加模型」按钮保持在滚动区外。
            // `.id()` 让列表成为 stateful 元素，`overflow_y_scroll` 才可用
            // （与弹框滚动区的写法一致）。
            let row_id = row.id;
            let mut list = div()
                .id(format!("model-list-{row_id}"))
                .flex()
                .flex_col()
                .gap_1()
                .when(row.models.len() > 4, |this| {
                    this.max_h(px(140.0)).overflow_y_scroll()
                });
            for model in &row.models {
                let model_id = model.id;
                let row_id = row.id;
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(
                            div().flex().flex_grow(1.5).child(
                                Input::new(&model.model_id)
                                    .aria_label(qcode_i18n::tr("ui.field.model_id")),
                            ),
                        )
                        .child(
                            div().flex().flex_grow(1.0).child(
                                Input::new(&model.name)
                                    .aria_label(qcode_i18n::tr("ui.field.display_name")),
                            ),
                        )
                        .child(
                            div().flex().flex_grow(1.0).child(
                                Input::new(&model.context_window)
                                    .aria_label(qcode_i18n::tr("ui.field.context_window")),
                            ),
                        )
                        .child(
                            div().flex().flex_grow(1.0).child(
                                Input::new(&model.max_tokens)
                                    .aria_label(qcode_i18n::tr("ui.field.max_tokens")),
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
            editor = editor.child(list);
            let row_id = row.id;
            editor = editor.child(
                Button::new(format!("add-model-{row_id}"))
                    .label(qcode_i18n::tr("ui.action.add_model"))
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
                qcode_i18n::tr("gui.setup.catalog.empty_warning").to_string()
            } else {
                qcode_i18n::trp(
                    "gui.setup.catalog.custom_ids",
                    &[badge.to_string(), ids.join(" / ")],
                )
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
                    .label(qcode_i18n::tr("ui.action.add_model"))
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
        // 标签现在来自运行期 locale，不再是编译期常量，所以收 `&str` 而不是
        // `&'static str`；`child` / `aria_label` 都要求 `'static`，所以在闭包里
        // 各转一次 `String`。这一处只影响设置页的静态表单，不在渲染热路径上。
        let field = |label: &str, input: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(sp(CAPTION_SIZE)).child(label.to_string()))
                .child(Input::new(input).aria_label(label.to_string()))
        };
        let mut body = div().flex().flex_col().gap_3();
        if let Some(route) = &self.custom_route {
            body = body.child(field("Provider ID", route));
        }
        if let Some(name) = &self.custom_name {
            body = body.child(field(&qcode_i18n::tr("ui.field.display_name"), name));
        }
        if let Some(base_url) = &self.custom_base_url {
            body = body.child(field(&qcode_i18n::tr("ui.field.api_base"), base_url));
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
                .child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .child(qcode_i18n::tr("gui.setup.custom.protocol")),
                )
                .child(protocol),
        );
        // Model drafts: only adopted on create.
        // 行列表最多显示 4 行（32px/行 + gap_1 = 140px），超过 4 行则纵向
        // 滚动；「添加模型」按钮保持在滚动区外。
        let mut model_rows = div()
            .id("custom-model-list")
            .flex()
            .flex_col()
            .gap_1()
            .when(self.custom_models.len() > 4, |this| {
                this.max_h(px(140.0)).overflow_y_scroll()
            });
        for model in &self.custom_models {
            let model_id = model.id;
            model_rows = model_rows.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(div().flex().flex_grow(1.5).child(
                        Input::new(&model.model_id).aria_label(qcode_i18n::tr("ui.field.model_id")),
                    ))
                    .child(div().flex().flex_grow(1.0).child(
                        Input::new(&model.name).aria_label(qcode_i18n::tr("ui.field.display_name")),
                    ))
                    .child(
                        div().flex().flex_grow(1.0).child(
                            Input::new(&model.context_window)
                                .aria_label(qcode_i18n::tr("ui.field.context_window")),
                        ),
                    )
                    .child(
                        div().flex().flex_grow(1.0).child(
                            Input::new(&model.max_tokens)
                                .aria_label(qcode_i18n::tr("ui.field.max_tokens")),
                        ),
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
        let mut models = div().flex().flex_col().gap_1().child(model_rows);
        models = models.child(
            Button::new("custom-add-model")
                .label(qcode_i18n::tr("ui.action.add_model"))
                .on_click(cx.listener(|this, _, window, cx| {
                    // 已有未填 id 的空行时不再追加：连点不会堆出一串空行。
                    let has_blank = this
                        .custom_models
                        .iter()
                        .any(|model| model.model_id.read(cx).value().trim().is_empty());
                    if !has_blank {
                        let model = this.model_row(window, cx, None);
                        this.custom_models.push(model);
                        cx.notify();
                    }
                })),
        );
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(sp(CAPTION_SIZE))
                                .child(qcode_i18n::tr("gui.setup.catalog.title")),
                        )
                        .child(div().flex_grow(1.0))
                        .child(
                            Button::new("custom-probe-models")
                                .label(qcode_i18n::tr("gui.setup.catalog.fetch"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.probe_custom_models(cx);
                                })),
                        ),
                )
                .child(models),
        );
        // 探测进行中/失败的结果行，复用编辑框同款文案。
        if let Some(result) = &self.probe_result {
            body = body.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(danger_color)
                    .child(result.to_string()),
            );
        }
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
                    .label(qcode_i18n::tr("ui.action.cancel"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.close_custom_card();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("create-custom-provider")
                    .label(qcode_i18n::tr("gui.setup.custom.create"))
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
                    .child(div().child(qcode_i18n::tr("gui.setup.custom.title")))
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
            let search = input(
                window,
                cx,
                String::new(),
                &qcode_i18n::tr("ui.field.search_model"),
            );
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
        // 按展示名（name）去重：厂商会给同一模型的多个版本 id 配同一个显示名，
        // 列表只展示一次，避免 `deepseek-v4-flash` 出现多行。
        let mut seen = std::collections::HashSet::new();
        let visible = candidates
            .iter()
            .filter(|candidate| {
                let pick = candidate_label(candidate);
                (query.is_empty()
                    || candidate.id.to_lowercase().contains(&query)
                    || candidate
                        .name
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(&query)))
                    && seen.insert(pick)
            })
            .collect::<Vec<_>>();
        let all_visible_picked = !visible.is_empty()
            && visible
                .iter()
                .all(|candidate| self.probe_picked.contains(&candidate_label(candidate)));
        let entity = cx.entity();
        let visible_ids = visible
            .iter()
            .map(|candidate| candidate_label(candidate))
            .collect::<Vec<_>>();
        let mut list = div()
            .id("probe-candidate-list")
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(320.0))
            .overflow_y_scroll();
        for candidate in &visible {
            let pick = candidate_label(candidate);
            let checked = self.probe_picked.contains(&pick);
            // 展示与勾选都用厂商显示名（name），写入 config 的也是它。
            let pick_clone = pick.clone();
            let entity = entity.clone();
            list = list.child(
                Checkbox::new(format!("probe-candidate-{}", candidate.id))
                    .checked(checked)
                    .label(pick)
                    .on_change(move |checked, _window, cx| {
                        entity.update(cx, |view, cx| {
                            if *checked {
                                view.probe_picked.insert(pick_clone.clone());
                            } else {
                                view.probe_picked.remove(&pick_clone);
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
                    .child(div().child(qcode_i18n::tr("gui.setup.picker.title")))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(muted_color)
                            .child(qcode_i18n::tr("gui.setup.picker.hint")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                div().flex_grow(1.0).child(
                                    Input::new(self.probe_search.as_ref().unwrap())
                                        .aria_label(qcode_i18n::tr("ui.field.search_model")),
                                ),
                            )
                            .child(
                                Button::new("probe-toggle-visible")
                                    .label(if all_visible_picked {
                                        qcode_i18n::tr("gui.setup.picker.clear_all")
                                    } else {
                                        qcode_i18n::tr("gui.setup.picker.select_all")
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
                            .child(
                                Button::new("close-probe-picker")
                                    .label(qcode_i18n::tr("ui.action.cancel"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.close_probe_picker();
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("adopt-probe-picked")
                                    .label(qcode_i18n::tr("gui.setup.picker.add_selected"))
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
            .map(|row| row.label())
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
                    .child(qcode_i18n::trp("gui.setup.delete.title", &[&display]))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(muted_color)
                            .child(qcode_i18n::trp("gui.setup.delete.body", &[&display])),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-delete-provider")
                                    .label(qcode_i18n::tr("ui.action.cancel"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.delete_target = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-delete-provider")
                                    .label(qcode_i18n::tr("ui.action.delete"))
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
            Some(model) => {
                qcode_i18n::trp("gui.setup.notice.default_with_model", &[provider, model])
            }
            None => qcode_i18n::trp("gui.setup.notice.default_provider", &[provider]),
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
    ///
    /// `lock_package` 为 true 时锁定套餐切换：编辑**已有** Provider 时套餐在
    /// 添加那一刻就确定了，编辑页不应再允许换套餐；picker 添加流程传 false，
    /// 让用户先选套餐再保存。
    fn open_editor(
        &mut self,
        route: &str,
        lock_package: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_probe();
        self.editing_provider = Some(route.to_string());
        self.editing_package_locked = lock_package;
        // 每次打开都从折叠态开始：对话框第一条只有 API 密钥。
        self.editor_expanded = false;
        self.provider_error = None;
        self.provider_notice = None;
        self.declaring = false;
        self.adding_provider = false;
        self.saved_notice = None;
        // 可配置 OAuth2 统一认证：打开时把 config 里的 oauth2 配置读进草稿。
        if qcode_kernel::providers::oauth2::is_oauth2_route(route) {
            self.init_oauth2_draft(route, window, cx);
        }
        cx.notify();
    }

    /// Close the provider edit dialog. `changed` reports whether the row was
    /// committed (the dialog auto-saves as you type; the flag only decides the
    /// saved-notice copy).
    fn close_editor(&mut self, changed: bool) {
        self.editing_provider = None;
        self.editing_package_locked = false;
        // 关闭编辑框即清理探测状态与对话框内提示：再次打开时不会残留
        // 上一次的探测结果/报错；探测线程的轮询也会因对话框关闭而丢弃
        // 结果（见 `start_probe`）。
        self.clear_probe();
        self.provider_error = None;
        self.provider_notice = None;
        if !changed {
            self.saved_notice = None;
        }
    }

    /// 切换一个 provider 卡片模型列表的展开/收起（纯 UI 状态）。
    fn toggle_provider_expanded(&mut self, route: &str, cx: &mut Context<Self>) {
        if !self.expanded_routes.remove(route) {
            self.expanded_routes.insert(route.to_string());
        }
        cx.notify();
    }

    /// 打开「添加模型」的 Provider 选择框：只做选择，不配置；选中某个
    /// Provider 后关闭选择框并进入该 Provider 的配置页。
    fn open_provider_picker(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // 编辑页或加模型弹框打开时该入口不可用（按钮已禁用）；事件误路由时
        // 直接忽略，避免把正在编辑的 provider 关掉（switch_package 重渲染会让
        // mouseup 落到「添加模型」按钮上）。
        if self.editing_provider.is_some() || self.adding_model_provider.is_some() {
            return;
        }
        self.adding_provider = true;
        self.editing_provider = None;
        self.declaring = false;
        self.saved_notice = None;
        cx.notify();
    }

    fn close_provider_picker(&mut self, cx: &mut Context<Self>) {
        self.adding_provider = false;
        cx.notify();
    }

    /// 选择框里点了一个 Provider：关闭选择框，未配置的 catalog route 先
    /// 写入预设行，再打开同一个编辑对话框。
    fn pick_provider(&mut self, route: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.close_provider_picker(cx);
        // 套餐单选：从 picker 进入厂商行时，把同厂商其他套餐行移出表格。
        // 否则保存时 `apply_tables` 会把残留套餐行也写进 config，出现
        // 同厂商多个 profile（见 `switch_package` 的同款清理）。
        if let Some((group, _)) = qcode_kernel::config::package_entry(route) {
            let group_routes = group
                .packages
                .iter()
                .map(|pkg| pkg.route)
                .collect::<Vec<_>>();
            if let Some(table) = self.tables.get_mut("providers") {
                table.rows.retain(|row| {
                    let name = cell(row, "name", cx).trim().to_string();
                    !group_routes.contains(&name.as_str())
                });
            }
        }
        // 用表格行而不是 `self.config` 决定要不要写预设行。`self.config`
        // 只在异步自动保存完成时才更新：刚删掉一个 provider 后它还残留着
        // 那条旧 profile，`pick_provider` 会因此跳过预设写入，卡片继续按
        // 那份残留状态渲染——「删了再添加，不配 key 也立刻冒出来」。
        // 表格才是「正在编辑哪条记录」的真源。
        let already_has_row = self
            .tables
            .get("providers")
            .map(|table| {
                table
                    .rows
                    .iter()
                    .any(|row| cell(row, "name", cx).trim() == route)
            })
            .unwrap_or(false);
        let restored_key = if !already_has_row {
            self.add_provider_preset(route, window, cx)
        } else {
            false
        };
        // picker 添加流程：套餐在编辑页里确定，先不锁定（`open_editor` 的
        // `lock_package=false`），让用户选好套餐再保存。
        self.open_editor(route, false, window, cx);
        // `open_editor` 会清掉提示，所以密钥恢复的提示在它之后设置。
        if restored_key {
            self.provider_notice =
                Some(qcode_i18n::trp("gui.setup.notice.key_restored", &[&route]));
        }
    }

    /// 选择框顶部的「自定义提供方…」：关闭选择框，进入自定义声明卡片。
    fn pick_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_provider_picker(cx);
        self.open_custom_card(window, cx);
    }

    /// 打开「为某 Provider 添加模型」的纯添加弹框（只含模型目录编辑器，
    /// 不含密钥/配置/删除），用于展开列表里的「＋ 配置模型」。
    fn open_add_model(&mut self, route: &str, cx: &mut Context<Self>) {
        self.clear_probe();
        self.adding_model_provider = Some(route.to_string());
        self.editing_provider = None;
        self.provider_error = None;
        self.provider_notice = None;
        cx.notify();
    }

    fn close_add_model(&mut self, cx: &mut Context<Self>) {
        self.adding_model_provider = None;
        cx.notify();
    }

    fn open_custom_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_probe();
        self.declaring = true;
        self.editing_provider = None;
        self.saved_notice = None;
        self.custom_route = Some(input(window, cx, String::new(), "Provider ID"));
        self.custom_name = Some(input(
            window,
            cx,
            String::new(),
            &qcode_i18n::tr("ui.field.display_name"),
        ));
        self.custom_base_url = Some(input(
            window,
            cx,
            String::new(),
            &qcode_i18n::tr("ui.field.api_base"),
        ));
        self.custom_key_env = Some(input(window, cx, String::new(), "Key env"));
        self.custom_protocol = "openai".to_string();
        self.custom_models = Vec::new();
        self.custom_error = None;
        cx.notify();
    }

    fn close_custom_card(&mut self) {
        self.declaring = false;
        self.clear_probe();
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
            self.custom_error = Some(qcode_i18n::tr("gui.setup.error.id_invalid").to_string());
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
            self.custom_error = Some(qcode_i18n::tr("gui.setup.error.id_taken").to_string());
            cx.notify();
            return;
        }
        if base_url.is_empty() {
            self.custom_error = Some(qcode_i18n::tr("gui.setup.error.base_required").to_string());
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
        self.saved_notice = Some(qcode_i18n::trp(
            "gui.setup.notice.created",
            &[&display_name],
        ));
        self.auto_save(cx, AutoSaveKind::Tables, false);
        cx.notify();
    }

    /// Abort any in-flight model probe: the surface it served (the provider
    /// editor, the custom declaration card, or the add-model dialog) has closed
    /// or switched — those surfaces are open one at a time, so a probe whose
    /// home is gone must not reopen a stale picker. The probe poll loop also
    /// notices via `probe_target` on its next tick, but clearing here stops it
    /// immediately.
    fn clear_probe(&mut self) {
        self.probe_rx = None;
        self.probe_result = None;
        self.probe_candidates = None;
        self.probe_picked.clear();
        self.probe_query.clear();
        self.probe_search = None;
        self.probe_target = None;
    }

    fn close_probe_picker(&mut self) {
        self.probe_candidates = None;
        self.probe_picked.clear();
        self.probe_query.clear();
        self.probe_search = None;
        self.probe_target = None;
    }
}

#[cfg(test)]
mod label_tests {
    // Named imports on purpose. `setup` glob-imports `gpui_kit::*`, which
    // re-exports GPUI's own `test` proc-macro attribute; `use super::*` would
    // pull it in, and every `#[test]` below would then expand to the GPUI macro
    // instead of the built-in attribute, recursing until the limit trips.
    use super::candidate_label;
    use qcode_providers::discovery::ModelCandidate;

    fn candidate(id: &str, name: Option<&str>) -> ModelCandidate {
        ModelCandidate {
            id: id.to_string(),
            name: name.map(str::to_string),
            ..ModelCandidate::default()
        }
    }

    /// The row shows the **vendor's display name**, and the id only when the
    /// listing carries no name — the picked value is written to the config as
    /// the model reference, and vendors accept the display name there.
    ///
    /// This used to assert `id — name`. Commit 5eb17dd changed
    /// `candidate_label` to show the name alone (so the list reads
    /// `deepseek-v4-flash` instead of the versioned `…-200910` id) but left this
    /// assertion behind, which made `cargo test -p qcode-gui --lib` red for
    /// every run after it. The behaviour, not the test, was the intent.
    #[test]
    fn a_display_name_wins_over_the_id() {
        assert_eq!(
            candidate_label(&candidate("deepseek-flash-200910", Some("deepseek-flash"))),
            "deepseek-flash"
        );
        // Nothing to show but the id: fall back to it.
        assert_eq!(
            candidate_label(&candidate("deepseek-flash", None)),
            "deepseek-flash"
        );
        // An empty name is the same as no name (`""` would render a blank row).
        assert_eq!(
            candidate_label(&candidate("deepseek-flash", Some(""))),
            "deepseek-flash"
        );
        // name == id: unchanged, but it must not double up.
        assert_eq!(
            candidate_label(&candidate("deepseek-flash", Some("deepseek-flash"))),
            "deepseek-flash"
        );
    }
}
