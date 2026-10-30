use super::*;

impl Render for SetupView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = Page::ALL[self.page];
        let mut body = div().flex().flex_col().gap_3().p_4();
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
                            "provider: {} · model: {}",
                            config.default_provider.as_deref().unwrap_or("(none)"),
                            config
                                .default_provider
                                .as_ref()
                                .and_then(|name| config.providers.get(name))
                                .and_then(|provider| provider.default_model.as_deref())
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
                            "default provider: {} · model: {}",
                            config.default_provider.as_deref().unwrap_or("(none)"),
                            config
                                .default_provider
                                .as_ref()
                                .and_then(|name| config.providers.get(name))
                                .and_then(|provider| provider.default_model.as_deref())
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
            for field in forms::fields_for(page) {
                body = body.child(self.render_field(field, cx));
            }
            for table in forms::tables_for(page) {
                body = body.child(self.render_table(table, cx));
            }
            for field in forms::GUI_FIELDS.iter().filter(|field| field.page == page) {
                body = body.child(self.render_field(field, cx));
            }
            if !self.wizard && matches!(page, Page::NetworkSandbox) {
                body = body.child(
                    "Sandbox and network settings apply immediately: the kernel is rebuilt on save.",
                );
            }
        }

        let mut nav = div().flex().flex_row().gap_1().p_2();
        if !self.wizard {
            for (index, item) in Page::ALL.iter().enumerate() {
                if *item == Page::Welcome {
                    continue;
                }
                let marker = if index == self.page { "●" } else { "○" };
                let title = if *item == Page::Review {
                    "About"
                } else {
                    item.title()
                };
                nav = nav.child(
                    Button::new(format!("settings-page-{index}"))
                        .label(format!("{marker} {title}"))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.page = index;
                            this.error = None;
                            cx.notify();
                        })),
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
                );
        }
        footer = footer.child(
            Button::new("setup-save")
                .label(if self.wizard { "Save & Start" } else { "Save" })
                .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
        );

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
            scroll.into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .p_2()
                    .child(if self.wizard {
                        format!("Setup {}/{}", self.page + 1, Page::ALL.len())
                    } else {
                        "Settings".to_string()
                    })
                    .child(if !self.wizard && page == Page::Review {
                        "About"
                    } else {
                        page.title()
                    }),
            )
            .child(nav)
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
                                    cx.notify();
                                });
                            }),
                    );
                }
                row.into_any_element()
            }
            _ => {
                let Some(input) = self.inputs.get(&key) else {
                    return div().into_any_element();
                };
                Input::new(input).aria_label(field.label).into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().child(field.label))
            .child(control)
            .child(div().child(field.help))
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
                                                    cx.notify();
                                                });
                                            }),
                                        );
                                    }
                                    select.into_any_element()
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
            table_view = table_view.child(
                "MCP env values are stored in config.toml. Prefer a minimal-scope token; Keychain-backed MCP secrets are not implemented yet.",
            );
        }
        if table.path == ["providers"] {
            let mut presets = div().flex().flex_row().gap_1();
            for preset in ["deepseek", "openai", "anthropic", "minimax", "ollama"] {
                presets = presets.child(
                    Button::new(format!("preset-{preset}"))
                        .label(format!("Add {preset}"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.add_provider_preset(preset, window, cx);
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
