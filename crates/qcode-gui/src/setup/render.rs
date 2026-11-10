use super::*;
use crate::CloseSettingsShortcut;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::Textarea;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::tooltip::Tooltip;

/// A small circular "?" glyph drawn by hand (a bordered circle with a "?"
/// inside). The Button component enforces a minimum size too large for this
/// affordance, so the trigger is a stateful div wrapped in a [`Selectable`]
/// — the contract the [`Popover`] trigger requires.
struct HelpCircle(Stateful<Div>);

impl Selectable for HelpCircle {
    fn selected(self, _selected: bool) -> Self {
        self
    }

    fn is_selected(&self) -> bool {
        false
    }
}

impl IntoElement for HelpCircle {
    type Element = <Stateful<Div> as IntoElement>::Element;

    fn into_element(self) -> Self::Element {
        self.0.into_element()
    }
}

impl Render for SetupView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = Page::ALL[self.page];
        // w_full：正文宽度固定为滚动区宽度，长文本（如 MCP 说明）才能自动换行，
        // 否则按内容自然宽度排布会把设置框撑大。
        let mut body = div().flex().flex_col().gap_3().p_4().w_full();
        if page == Page::Welcome {
            body = body
                .child(TextView::markdown(
                    "setup-welcome",
                    "# Welcome to qcode\n\nThis wizard creates the config file and keeps every setting editable later.",
                ))
                .child(format!("Config: {}", self.path.display()))
                .child(format!("Data dir: {}", self.data_dir.display()));
            if let Some(repair) = &self.repair {
                body = body
                    .child(format!(
                        "existing config could not be parsed: {repair}\nSaving will keep a .bak backup before replacing it."
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                Button::new("repair-editor")
                                    .label("Open in editor")
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        if let Err(error) = open_in_editor(&this.path) {
                                            this.error = Some(error);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Button::new("repair-quit")
                                    .label("Quit")
                                    .on_click(cx.listener(|_this, _, _window, cx| cx.quit())),
                            ),
                    );
            }
        } else if page == Page::Review && !self.wizard {
            body = body
                .child(TextView::markdown(
                    "settings-about",
                    "## About\n\nqcode desktop. Saving applies to the open window immediately: it rebuilds the kernel in place.",
                ))
                .child(format!("qcode {}", env!("CARGO_PKG_VERSION")))
                .child(format!("Config: {}", self.path.display()))
                .child(format!("Data dir: {}", self.data_dir.display()));
            match self.build_config(cx, false) {
                Ok(config) => {
                    body = body
                        .child(format!(
                            "default model: {} · {}",
                            config.default_model.provider.as_deref().unwrap_or("(none)"),
                            config
                                .default_model
                                .model
                                .as_deref()
                                .unwrap_or("(provider default)")
                        ))
                        .child(format!(
                            "providers: {} · pricing: {} · approval rules: {} · MCP servers: {}",
                            config.providers.len(),
                            config.pricing.len(),
                            config.approval.rules.len(),
                            config.mcp.servers.len()
                        ));
                }
                Err(error) => body = body.child(format!("cannot read config: {error}")),
            }
            if let Some(repair) = &self.repair {
                body = body.child(format!("config warning: {repair}"));
            }
            if let Some(error) = &self.error {
                body = body.child(format!("error: {error}"));
            }
            if let Some(info) = &self.info {
                body = body.child(format!("info: {info}"));
            }
        } else if page == Page::Review {
            body = body
                .child(TextView::markdown(
                    "setup-review",
                    "## Review\n\nSave to write the config atomically. A `.bak` backup is kept when the file already exists.",
                ))
                .child(format!("Config: {}", self.path.display()))
                .child(format!("Data dir: {}", self.data_dir.display()))
                .child(format!("Saved: {}", self.saved));
            match self.build_config(cx, false) {
                Ok(config) => {
                    body = body
                        .child(format!(
                            "default model: {} · {}",
                            config.default_model.provider.as_deref().unwrap_or("(none)"),
                            config
                                .default_model
                                .model
                                .as_deref()
                                .unwrap_or("(provider default)")
                        ))
                        .child(format!(
                            "providers: {} · pricing: {} · approval rules: {} · MCP servers: {}",
                            config.providers.len(),
                            config.pricing.len(),
                            config.approval.rules.len(),
                            config.mcp.servers.len()
                        ))
                        .child(format!(
                            "sandbox: {:?} · network: {:?}",
                            config.sandbox.mode, config.network.mode
                        ));
                }
                Err(error) => body = body.child(format!("cannot preview config: {error}")),
            }
            if let Some(repair) = &self.repair {
                body = body.child(format!("repair warning: {repair}"));
            }
            if let Some(error) = &self.error {
                body = body.child(format!("error: {error}"));
            }
            if let Some(info) = &self.info {
                body = body.child(format!("info: {info}"));
            }
        } else {
            if !self.wizard {
                body = body.child(TextView::markdown(
                    "settings-section",
                    format!("## {}\n\n{}", page.title(), section_intro(page)),
                ));
            }
            if page == Page::Models && !self.wizard {
                // Models section (settings mode): the default-model summary
                // line then the provider card list with one inline editor at a
                // time. The three legacy default text fields were removed in
                // V2.1: the default is pinned per card via "设为默认".
                body = body.child(self.render_models_providers(cx));
            } else {
                if page == Page::Models {
                    // Read-only catalog directory (wizard mode keeps the
                    // original table form): every route the resolver knows,
                    // its wire protocol, whether a profile is configured and
                    // its credential state (key stored / missing / keyless).
                    let directory = qcode_kernel::provider_registry::ProviderRegistry::new(None)
                        .list_configurable_providers(&self.config);
                    body = body.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().child("Provider directory"))
                            .children(directory.into_iter().map(|row| {
                                let credential = match row.credential_state {
                                    qcode_kernel::provider_registry::CredentialState::Configured => {
                                        "已存"
                                    }
                                    qcode_kernel::provider_registry::CredentialState::Missing => "缺失",
                                    qcode_kernel::provider_registry::CredentialState::Keyless => {
                                        "无需 key"
                                    }
                                };
                                format!(
                                    "{} ({}) · {} · {} · 凭证: {}",
                                    row.route,
                                    row.display_name,
                                    row.protocol.as_str(),
                                    if row.configured {
                                        "configured"
                                    } else {
                                        "catalog default"
                                    },
                                    credential
                                )
                            })),
                    );
                }
                for field in forms::fields_for(page) {
                    body = body.child(self.render_field(field, cx));
                }
                for table in forms::tables_for(page) {
                    body = body.child(self.render_table(table, cx));
                }
            }
            for field in forms::GUI_FIELDS.iter().filter(|field| field.page == page) {
                body = body.child(self.render_field(field, cx));
            }
            if !self.wizard && matches!(page, Page::NetworkSandbox) {
                body = body.child(
                    "Sandbox and network settings apply immediately: each change is saved as you make it.",
                );
            }
        }

        let mut footer = div().flex().flex_row().gap_2().p_2();
        if self.wizard {
            footer = footer
                .child(
                    Button::new("setup-back")
                        .label("Back")
                        .on_click(cx.listener(|this, _, _window, cx| this.back(cx))),
                )
                .child(
                    Button::new("setup-next")
                        .label("Next")
                        .on_click(cx.listener(|this, _, _window, cx| this.next(cx))),
                )
                .child(
                    Button::new("setup-save")
                        .label("Save & Start")
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                );
        } else {
            // Settings mode saves every change as it happens (per-section auto
            // save, design SETTINGS-UI-V2 §4.5); no Save button. 说明文字用小号
            // 灰色，弱化它是状态提示而不是可操作项。
            footer = footer.child(
                div()
                    .flex()
                    .items_center()
                    .text_size(crate::sp(crate::CAPTION_SIZE))
                    .text_color(cx.theme().muted_foreground)
                    .child("自动保存 · 修改即时写入"),
            );
        }

        let mut steps = div().w(px(200.0)).h_full().flex().flex_col().gap_1().p_2();
        if self.wizard {
            for (index, item) in Page::ALL.iter().enumerate() {
                let marker = if index == self.page {
                    "▶"
                } else if index < self.page {
                    "✓"
                } else {
                    "·"
                };
                steps = steps.child(
                    Button::new(format!("setup-step-{index}"))
                        .label(format!("{marker} {index}. {}", item.title()))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.page = index;
                            this.error = None;
                            cx.notify();
                        })),
                );
            }
        }
        let scroll = div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .min_h_0()
            .id(if self.wizard {
                "setup-scroll"
            } else {
                "settings-scroll"
            })
            .overflow_y_scroll()
            .child(body);
        let content = if self.wizard {
            div()
                .flex()
                .flex_row()
                .flex_grow(1.0)
                .min_h_0()
                .child(steps)
                .child(scroll)
                .into_any_element()
        } else {
            // Settings mode: a left nav rail (each page once; Welcome is
            // wizard-only, Review shows as About) next to the scrolling body.
            // Borderless ghost buttons: unselected on the window's own white
            // fill, selected with the grey active tint — no leading dots.
            let mut rail = div().w(px(180.0)).h_full().flex().flex_col().gap_1().p_2();
            for (index, item) in Page::ALL.iter().enumerate() {
                if *item == Page::Welcome {
                    continue;
                }
                let title = if *item == Page::Review {
                    "About"
                } else {
                    item.title()
                };
                rail = rail.child(
                    Button::new(format!("settings-page-{index}"))
                        .label(title)
                        .ghost()
                        .h(px(36.0))
                        .selected(index == self.page)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.page = index;
                            this.error = None;
                            cx.notify();
                        })),
                );
            }
            div()
                .flex()
                .flex_row()
                .flex_grow(1.0)
                .min_h_0()
                .child(rail)
                .child(scroll)
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            // Escape closes the window, but only in settings mode: the wizard's Escape
            // would mean "cancel setup", which nobody asked for, so it gets no key
            // context and keeps the platform's default behaviour.
            .when(!self.wizard, |this| {
                this.key_context("Settings").on_action(cx.listener(
                    |this, _: &CloseSettingsShortcut, window, cx| {
                        // Same path as the X button: flush the pending edit first, and
                        // stay open (with the reason on screen) if the write failed.
                        if !this.prepare_to_close(cx) {
                            return;
                        }
                        crate::setup::close_settings_window(window, cx);
                    },
                ))
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .p_2()
                    .child(if self.wizard {
                        div().text_size(crate::sp(18.0)).child(format!(
                            "Setup {}/{}",
                            self.page + 1,
                            Page::ALL.len()
                        ))
                    } else {
                        // 设置窗口标题固定为「设置」，不再跟随左侧导航动态切换。
                        // 比向导头部小一档，并向右让出一点间距。
                        div().text_size(crate::sp(16.0)).pl_3().child("设置")
                    })
                    .child(if self.wizard { page.title() } else { "" }),
            )
            .children(
                self.error
                    .as_ref()
                    .map(|error| div().px_2().child(format!("error: {error}"))),
            )
            .children(
                self.info
                    .as_ref()
                    .map(|info| div().px_2().child(format!("info: {info}"))),
            )
            .child(content)
            .child(footer)
            // Modal overlays (settings mode only): the provider edit dialog,
            // the custom-provider declaration dialog, the probe candidate
            // picker and the provider delete confirmation.
            .child(self.render_provider_dialog(cx))
            .child(self.render_custom_card(cx))
            .child(self.render_probe_picker(window, cx))
            .child(self.render_delete_dialog(cx))
            // Ctrl+= / Ctrl+- (Cmd+= / Cmd+- on macOS) zoom the whole UI's text
            // while this window has focus; the key bindings are app-global, the
            // handlers are per-window (see `SetupView::zoom_font`).
            .on_action(cx.listener(|this, _: &crate::ZoomInShortcut, window, cx| {
                this.zoom_font(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::ZoomOutShortcut, window, cx| {
                this.zoom_font(-1, window, cx);
            }))
    }
}

/// One-line intro under the `## {title}` section header, shown for every
/// settings section (settings mode only).
fn section_intro(page: Page) -> &'static str {
    match page {
        Page::Models => "配置默认模型与各模型供应商",
        Page::Agent => "上下文、回合预算与消息行为",
        Page::NetworkSandbox => "沙箱与网络出口策略",
        Page::Search => "联网搜索后端与密钥",
        Page::Mcp => "stdio MCP 服务器",
        Page::Skills => "Agent 技能",
        Page::ApprovalPricing => "审批规则与 token 定价",
        Page::Appearance => "主题、字号与界面偏好",
        Page::Update => "版本检查与更新源",
        Page::Welcome | Page::Review => "",
    }
}

impl SetupView {
    pub(crate) fn render_field(
        &self,
        field: &'static FieldSpec,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = field.path.join(".");
        let control = match field.kind {
            FieldKind::Bool => Switch::new(format!("switch-{key}"))
                .checked(self.bools.get(&key).copied().unwrap_or(false))
                .on_change({
                    let key = key.clone();
                    let entity = cx.entity();
                    move |checked, _window, cx| {
                        entity.update(cx, |view, cx| {
                            view.bools.insert(key.clone(), *checked);
                            view.auto_save(cx, AutoSaveKind::Scalar(field.path), false);
                            cx.notify();
                        });
                    }
                })
                .into_any_element(),
            FieldKind::Select(options) => {
                let current = self.selects.get(&key).cloned().unwrap_or_default();
                let entity = cx.entity();
                let mut row = div().flex().flex_row().gap_1();
                for option in options {
                    let marker = if current == *option { "●" } else { "○" };
                    let key = key.clone();
                    let value = option.to_string();
                    let entity = entity.clone();
                    row = row.child(
                        Button::new(format!("select-{key}-{option}"))
                            .label(format!("{marker} {option}"))
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |view, cx| {
                                    view.selects.insert(key.clone(), value.clone());
                                    view.auto_save(cx, AutoSaveKind::Scalar(field.path), false);
                                    cx.notify();
                                });
                            }),
                    );
                }
                row.into_any_element()
            }
            FieldKind::Lines => {
                let Some(textarea) = self.textareas.get(&key) else {
                    return div().into_any_element();
                };
                Textarea::new(textarea)
                    .h(px(72.0))
                    .aria_label(field.label)
                    .into_any_element()
            }
            _ => {
                let Some(input) = self.inputs.get(&key) else {
                    return div().into_any_element();
                };
                Input::new(input).aria_label(field.label).into_any_element()
            }
        };
        let theme = cx.theme();
        let (muted_foreground, border_color) = (theme.muted_foreground, theme.border);
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .py_1()
            .border_b_1()
            .border_color(rgba(0x7f7f7f20))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(div().child(field.label))
                    // The help text floats over a popover on click, keeping the
                    // first version's tooltip visuals (a click trigger instead
                    // of hover). `appearance(false)` drops the popover's own
                    // surface so the Tooltip content is the only floating layer.
                    .child(
                        Popover::new(format!("help-{key}"))
                            .anchor(Anchor::BottomLeft)
                            .offset(px(4.0))
                            .appearance(false)
                            .trigger(HelpCircle(
                                div()
                                    .id(format!("help-trigger-{key}"))
                                    .size(px(10.0))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(border_color)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .text_size(px(8.0))
                                    .text_color(muted_foreground)
                                    .child("?"),
                            ))
                            .content(move |_state, window, cx| {
                                Tooltip::new(field.help)
                                    .build(window, cx)
                                    .into_any_element()
                            }),
                    ),
            )
            .child(control)
            .into_any_element()
    }

    pub(crate) fn render_table(
        &mut self,
        table: &'static TableSpec,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = table.path.join(".");
        let rows = self
            .tables
            .get(&key)
            .map(|state| {
                state
                    .rows
                    .iter()
                    .map(|row| {
                        let mut card = div().flex().flex_col().gap_1().p_2();
                        for column in table.columns {
                            let control = match column.kind {
                                FieldKind::Bool => {
                                    Switch::new(format!("table-{key}-{}-{}", row.id, column.key))
                                        .checked(*row.bools.get(column.key).unwrap_or(&false))
                                        .on_change({
                                            let table_key = key.clone();
                                            let column_key = column.key;
                                            let row_id = row.id;
                                            let entity = cx.entity();
                                            move |checked, _window, cx| {
                                                entity.update(cx, |view, cx| {
                                                    if let Some(table) =
                                                        view.tables.get_mut(&table_key)
                                                        && let Some(row) = table
                                                            .rows
                                                            .iter_mut()
                                                            .find(|row| row.id == row_id)
                                                    {
                                                        row.bools.insert(column_key, *checked);
                                                    }
                                                    view.auto_save(cx, AutoSaveKind::Tables, false);
                                                    cx.notify();
                                                });
                                            }
                                        })
                                        .into_any_element()
                                }
                                FieldKind::Select(options) => {
                                    let current =
                                        row.selects.get(column.key).cloned().unwrap_or_default();
                                    let entity = cx.entity();
                                    let mut select = div().flex().flex_row().gap_1();
                                    for option in options {
                                        let marker = if current == *option { "●" } else { "○" };
                                        let table_key = key.clone();
                                        let column_key = column.key;
                                        let row_id = row.id;
                                        let value = option.to_string();
                                        let entity = entity.clone();
                                        select = select.child(
                                            Button::new(format!(
                                                "table-{key}-{}-{column_key}-{option}",
                                                row.id
                                            ))
                                            .label(format!("{marker} {option}"))
                                            .on_click(move |_, _, cx| {
                                                entity.update(cx, |view, cx| {
                                                    if let Some(table) =
                                                        view.tables.get_mut(&table_key)
                                                        && let Some(row) = table
                                                            .rows
                                                            .iter_mut()
                                                            .find(|row| row.id == row_id)
                                                    {
                                                        row.selects
                                                            .insert(column_key, value.clone());
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
                                    let Some(textarea) = row.textareas.get(column.key) else {
                                        continue;
                                    };
                                    Textarea::new(textarea)
                                        .h(px(72.0))
                                        .aria_label(column.label)
                                        .into_any_element()
                                }
                                _ => {
                                    let Some(input) = row.cells.get(column.key) else {
                                        continue;
                                    };
                                    Input::new(input)
                                        .aria_label(column.label)
                                        .into_any_element()
                                }
                            };
                            card = card.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().child(column.label))
                                    .child(control),
                            );
                        }
                        if table.path == ["providers"]
                            && let Some(secret) = row.cells.get("__key")
                        {
                            let key_row_id = row.id;
                            card = card.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .child("API key (stored in Keychain, not config.toml)"),
                                    )
                                    .child(
                                        Input::new(secret)
                                            .aria_label("Provider API key for Keychain"),
                                    )
                                    .child(
                                        Button::new(format!("store-key-{key}-{key_row_id}"))
                                            .label("Store key")
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.store_provider_key(key_row_id, window, cx);
                                            })),
                                    ),
                            );
                        }
                        let table_key = key.clone();
                        let row_id = row.id;
                        card.child(
                            Button::new(format!("remove-{key}-{row_id}"))
                                .label("Remove")
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.remove_row(&table_key, row_id, cx);
                                })),
                        )
                        .into_any_element()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let table_key = key.clone();
        let mut table_view = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().child(table.title))
            .children(rows);
        if table.path == ["mcp", "servers"] {
            // 说明文字限制在表格宽度内并允许换行，避免单行长文本撑大设置框。
            table_view = table_view.child(
                div()
                    .w_full()
                    .text_size(crate::sp(crate::CAPTION_SIZE))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "MCP env values are stored in config.toml. Prefer a minimal-scope token; Keychain-backed MCP secrets are not implemented yet.",
                    ),
            );
        }
        if table.path == ["providers"] {
            let mut presets = div().flex().flex_row().gap_1();
            for entry in qcode_kernel::config::catalog_entries() {
                let route = entry.route;
                presets = presets.child(
                    Button::new(format!("preset-{route}"))
                        .label(format!("Add {}", entry.display_name))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.add_provider_preset(route, window, cx);
                        })),
                );
            }
            table_view = table_view.child(presets);
        }
        table_view
            .child(
                Button::new(format!("add-{key}"))
                    .label("Add custom row")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let spec = forms::TABLES
                            .iter()
                            .find(|spec| spec.path.join(".") == table_key)
                            .expect("table spec");
                        this.add_row(spec, window, cx);
                    })),
            )
            .into_any_element()
    }
}
