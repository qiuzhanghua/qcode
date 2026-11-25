use super::*;
use crate::CloseSettingsShortcut;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::Textarea;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::text::markdown_ast;
use gpui_kit::component::text::{
    InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
    MarkdownPlugin,
};

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
        // 主窗口模型菜单「切即默认」会直接写 config.toml；本窗口没有自己的
        // 事件流，在这里幂等地起一个低频 mtime ticker，检测到外部写入就刷新
        // 内存 config，让默认 Provider 的标记与主窗口保持同步（render 至少会
        // 跑一次，ticker 只 spawn 一次）。
        if !self.config_ticker_running {
            self.start_config_ticker(cx);
        }
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
                                    .label(qcode_i18n::tr("gui.setup.open_in_editor"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        if let Err(error) = open_in_editor(&this.path) {
                                            this.error = Some(error);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Button::new("repair-quit")
                                    .label(qcode_i18n::tr("gui.setup.action.quit"))
                                    .on_click(cx.listener(|_this, _, _window, cx| cx.quit())),
                            ),
                    );
            }
        } else if page == Page::Review && !self.wizard {
            body = body
                .child(TextView::markdown(
                    "settings-about",
                    "## About\n\nqcode desktop. Saving applies to the open window when every session \
                     is idle: the kernel is rebuilt in place, no restart and no second window. A \
                     turn that is still running finishes first.",
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
                            .child(
                                div().child(qcode_i18n::tr("gui.setup.provider_directory.title")),
                            )
                            .children(directory.into_iter().map(|row| {
                                // 凭证三态复用 `gui.setup.credential.*`：这里原先是
                                // 「已存 / 缺失 / 无需 key」的短说法，与 Provider 卡片上
                                // 的「凭证已配置 / 凭证缺失 / 无需密钥」是**同一个概念**。
                                // 留着两套只会让英文也分叉两次，所以统一到一套。
                                let credential =
                                    super::models::credential_text(row.credential_state);
                                let profile = if row.configured {
                                    qcode_i18n::tr("gui.setup.provider_directory.state.configured")
                                } else {
                                    qcode_i18n::tr(
                                        "gui.setup.provider_directory.state.catalog_default",
                                    )
                                };
                                qcode_i18n::trp(
                                    "gui.setup.provider_directory.line",
                                    &[
                                        row.route.as_str(),
                                        row.display_name.as_str(),
                                        row.protocol.as_str(),
                                        profile.as_ref(),
                                        credential.as_ref(),
                                    ],
                                )
                            })),
                    );
                }
                // Network & Sandbox 页分两段排布：沙箱（含审批）在上、网络出口
                // 在下，中间一条横线隔开。其余页面仍按 FIELDS 的定义顺序平铺。
                if page == Page::NetworkSandbox {
                    // 「网络出口」组随 `sandbox.network` 变化：只有代理启动
                    // （proxy/strict）时白名单、私有网段、未知主机策略和审计才有
                    // 执行者；off 由沙箱直接断网、open 直连裸网络，整组换成一行
                    // 说明，免得把死配置展示成可调项。旧写法 `[network] mode` /
                    // `enabled` 不展示：老文件的兼容由加载逻辑推导，保存不改写它们。
                    let network = self
                        .selects
                        .get("sandbox.network")
                        .cloned()
                        .unwrap_or_default();
                    body = body.child(group_heading(qcode_i18n::tr("gui.setup.group.sandbox")));
                    for path in forms::NETWORK_SANDBOX_SANDBOX_GROUP {
                        let Some(field) = forms::field_at(path) else {
                            continue;
                        };
                        body = body.child(self.render_field(field, cx));
                    }
                    body = body.child(div().h(px(1.0)).w_full().my_1().bg(cx.theme().border));
                    body = body.child(group_heading(qcode_i18n::tr("gui.setup.group.network")));
                    let egress = forms::network_egress_fields(&network);
                    if egress.is_empty() {
                        let reason = if network == "open" {
                            qcode_i18n::tr("gui.setup.network.open_note")
                        } else {
                            qcode_i18n::tr("gui.setup.network.off_note")
                        };
                        body = body.child(
                            div()
                                .py_1()
                                .text_size(crate::sp(crate::CAPTION_SIZE))
                                .text_color(cx.theme().muted_foreground)
                                .child(reason),
                        );
                    } else {
                        for path in egress {
                            let Some(field) = forms::field_at(path) else {
                                continue;
                            };
                            body = body.child(self.render_field(field, cx));
                        }
                    }
                } else {
                    for field in forms::fields_for(page) {
                        body = body.child(self.render_field(field, cx));
                        // web_search 已启用但缺必填项：警示紧跟在「搜索后端」切换
                        // 下面，避免沉在页面底部（下面还有密钥/命令/URL 三组输入）
                        // 被滚出视线。内核每次重建都会在日志里 WARN「the tool is
                        // not registered」，这里直接说清还缺什么；向导模式下
                        // `validate_page` 还会拦下「下一步」。
                        if field.path == ["search", "backend"] && page == Page::Search {
                            let (enabled, backend, api_key_env, api_key_command, base_url) =
                                search_form_values(self, cx);
                            if enabled
                                && !search_usable(
                                    backend,
                                    &api_key_env,
                                    &api_key_command,
                                    &base_url,
                                    &|key| std::env::var(key).ok(),
                                )
                            {
                                body = body.child(
                                    div()
                                        .w_full()
                                        .text_size(crate::sp(crate::CAPTION_SIZE))
                                        .text_color(cx.theme().danger)
                                        .child(search_missing_message(backend)),
                                );
                            }
                        }
                    }
                }
                for table in forms::tables_for(page) {
                    body = body.child(self.render_table(table, cx));
                }
            }
            for field in forms::GUI_FIELDS.iter().filter(|field| field.page == page) {
                body = body.child(self.render_field(field, cx));
            }
            if !self.wizard && page == Page::Appearance {
                body = body.child(self.render_shell_environment_probe(cx));
            }
            if !self.wizard && matches!(page, Page::NetworkSandbox) {
                // ADR-0103 §3：措辞是事实，不是期望。换配置要重建整个 kernel，
                // 而重建只在所有会话空闲时发生（`shell/mod.rs` 的 `pending_rebuild`），
                // 所以「立即生效」从来不成立；原句是这么写的，已改。
                //
                // 另半句来自 ADR-0103 §4：即使换进去了，也是**对下一个进程**生效——
                // Landlock 是进程级不可逆降权，已经在跑的 `run_shell` 及其子孙不会
                // 因为这次收紧而失去已有权限。不说清这点，用户会以为「点了就安全了」。
                body = body.child(
                    "Sandbox and network settings are saved as you change them, and take effect \
                     once every session is idle — the kernel is rebuilt then, never mid-turn. \
                     Confinement applies to processes started after that: a `run_shell` already \
                     running keeps the grants it started with.",
                );
                // ADR-0104 §7: `strict` is the one combination that can never
                // assemble outside macOS. The select hides it for other backends,
                // but a file written before this rule could still carry it —
                // say so instead of letting assembly fail with a bare error.
                let mode = self
                    .selects
                    .get("sandbox.mode")
                    .cloned()
                    .unwrap_or_default();
                let network = self
                    .selects
                    .get("sandbox.network")
                    .cloned()
                    .unwrap_or_default();
                if network == "strict" && mode != "sandbox-exec" {
                    body = body.child(
                        div()
                            .text_size(crate::sp(crate::CAPTION_SIZE))
                            .text_color(cx.theme().muted_foreground)
                            .child(qcode_i18n::trp("gui.setup.network.strict_mode", &[&mode])),
                    );
                }
            }
        }

        let mut footer = div().flex().flex_row().gap_2().p_2();
        if self.wizard {
            footer = footer
                .child(
                    Button::new("setup-back")
                        .label(qcode_i18n::tr("gui.setup.action.back"))
                        .on_click(cx.listener(|this, _, _window, cx| this.back(cx))),
                )
                .child(
                    Button::new("setup-next")
                        .label(qcode_i18n::tr("gui.setup.action.next"))
                        .on_click(cx.listener(|this, _, _window, cx| this.next(cx))),
                )
                .child(
                    Button::new("setup-save")
                        .label(qcode_i18n::tr("gui.setup.save_and_start"))
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
                    .child(qcode_i18n::tr("gui.setup.autosave")),
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
            .min_w_0()
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
            // 左导航轨固定 180px 宽。`flex_shrink` 默认 1.0，不带这一句时，
            // 右侧正文内容一宽（Network & Sandbox 的宽输入框、About 的长行）
            // 就会把 rail 压缩变窄——正文可以收缩（scroll 有 min_w_0），
            // 导航轨不该跟着缩。
            let mut rail = div()
                .w(px(180.0))
                .h_full()
                .flex()
                .flex_shrink(0.0)
                .flex_col()
                .gap_1()
                .p_2();
            for (index, item) in Page::ALL.iter().enumerate() {
                if *item == Page::Welcome {
                    continue;
                }
                // 设置模式（非向导）里最后一页讲的是「关于本程序」，不是向导的
                // 「检查一遍」。文案另立 key：它和 `Page::Review` 的向导名字不同义。
                let title = if *item == Page::Review {
                    qcode_i18n::tr("gui.setup.page.about")
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
                        div()
                            .text_size(crate::sp(16.0))
                            .pl_3()
                            .child(qcode_i18n::tr("gui.setup.window_title"))
                    })
                    .child(if self.wizard { page.title() } else { "".into() }),
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
            // the 「添加模型」provider picker, the custom-provider declaration
            // dialog, the probe candidate picker and the delete confirmation.
            .child(self.render_provider_dialog(cx))
            .child(self.render_add_model_dialog(cx))
            .child(self.render_provider_picker(cx))
            .child(self.render_custom_card(cx))
            .child(self.render_probe_picker(window, cx))
            .child(self.render_delete_dialog(cx))
            .child(self.render_sandbox_blocked_dialog(cx))
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
fn section_intro(page: Page) -> std::borrow::Cow<'static, str> {
    use qcode_i18n::tr;
    match page {
        Page::Models => tr("gui.setup.section_intro.models"),
        Page::Agent => tr("gui.setup.section_intro.agent"),
        Page::NetworkSandbox => tr("gui.setup.section_intro.network_sandbox"),
        Page::Search => tr("gui.setup.section_intro.search"),
        Page::Mcp => tr("gui.setup.section_intro.mcp"),
        Page::Skills => tr("gui.setup.section_intro.skills"),
        Page::ApprovalPricing => tr("gui.setup.section_intro.approval_pricing"),
        Page::Appearance => tr("gui.setup.section_intro.appearance"),
        Page::Update => tr("gui.setup.section_intro.update"),
        // 欢迎页与确认页的标题下不写引言，不是「空字符串译文」。
        Page::Welcome | Page::Review => "".into(),
    }
}

/// 设置页分组的小标题（Network & Sandbox 用）：secondary 字号、半粗，
/// 与页头 `## 标题` 拉开层级。
fn group_heading(title: impl Into<gpui::SharedString>) -> Div {
    div()
        .pt_1()
        .text_size(crate::sp(crate::SECONDARY_SIZE))
        .font_weight(FontWeight::SEMIBOLD)
        .child(title.into())
}

/// 行内问号圆圈携带的数据：哪个键（hover_help 的键）与它的帮助文本。
struct HelpInlineData {
    key: String,
    /// 已经翻译好的帮助文本（节点在构造时就译好，渲染阶段只读它）。
    help: std::borrow::Cow<'static, str>,
}

/// 该节点是否是 `<qc-help/>` 行内 HTML 标记（独立函数便于直接测试）。
pub(super) fn is_help_marker(node: &markdown_ast::Node) -> bool {
    matches!(
        node,
        markdown_ast::Node::Html(html) if html.value == "<qc-help/>"
    )
}

/// 把 `<qc-help/>` 行内 HTML 标记替换为悬浮提示圆圈（MarkdownPlugin）。
///
/// 为什么走 markdown 行内元素管线：普通 div 布局做不到「圆圈跟在换行标签
/// 最后一个字后面」——换行的文本盒占满整列，圆圈作为兄弟元素只能对齐盒子
/// 右缘。把圆圈做成文本行内元素（InlineElement）后它随文字一起换行，
/// 永远停在最后一个字符后面。
struct HelpInlinePlugin {
    entity: Entity<SetupView>,
    key: String,
    /// 帮助文本的**译文 key**（`gui.setup.spec.<path>.help`）。在这里翻译而不是
    /// 在数据表里存英文原文：数据表是 `const`，译不了语言。
    help: &'static str,
}

impl MarkdownPlugin for HelpInlinePlugin {
    fn name(&self) -> &str {
        "qc-help"
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        _cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        // 标签文案里不可能出现 `<qc-help/>`，匹配它是安全的。解析在后台任务
        // 上运行，不能碰 Window/App，只用节点值判断。
        if is_help_marker(node) {
            Some(MarkdownNode::new(
                "qc-help",
                HelpInlineData {
                    key: self.key.clone(),
                    help: qcode_i18n::tr(self.help),
                },
            ))
        } else {
            None
        }
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let data = node.data::<HelpInlineData>()?;
        let theme = cx.theme();
        let open = self.entity.read(cx).hover_help.as_deref() == Some(data.key.as_str());
        let popover = help_popover(
            self.entity.clone(),
            data.key.clone(),
            data.help.clone(),
            open,
            theme.muted_foreground,
            theme.border,
        );
        let circle = InlineElement::new(popover.into_any_element());
        // 默认对齐把元素底边贴到行底（descent 侧），14px 圆圈因此在行内
        // 明显偏低。行内图片用的居中算法是 size.height/2 + 正文基线 - 行高/2；
        // 正文基线即文字在其行框内的基线（centered_baseline），用正文的
        // 字号与行高重新 shape 一行就能拿到。
        let line_height = context.line_height();
        let body = window.text_system().shape_line(
            " ".into(),
            context.font_size(),
            &[context.text_style().to_run(1)],
            None,
        );
        let body_baseline = (line_height - body.ascent - body.descent) / 2. + body.ascent;
        let baseline = (px(7.0) + body_baseline - line_height / 2.).max(Pixels::ZERO);
        Some(circle.with_baseline(baseline))
    }
}

/// 问号圆圈的悬浮提示 popover：hover 触发（`hover_help` 受控）、移开/Esc/
/// 点外部关闭。Split 布局（兄弟元素）与 Inline 布局（文本行内元素）共用。
/// 帮助气泡。`help` 是**已翻译**的文本：数据表里存的是 key，翻译发生在构造
/// `HelpInlineData` 的时候（数据表是 `const`，译不了语言）。
fn help_popover(
    entity: Entity<SetupView>,
    key: String,
    help: std::borrow::Cow<'static, str>,
    open: bool,
    muted_foreground: Hsla,
    border_color: Hsla,
) -> Popover {
    let open_entity = entity.clone();
    let hover_entity = entity.clone();
    let hover_key = key.clone();
    let id = format!("help-{key}");
    let trigger_id = format!("help-trigger-{key}");
    Popover::new(id)
        .anchor(Anchor::BottomLeft)
        .offset(px(1.5))
        .appearance(false)
        .open(open)
        .on_open_change(move |open, _window, cx| {
            let entity = open_entity.clone();
            entity.update(cx, |view, cx| {
                if !*open {
                    view.hover_help = None;
                    cx.notify();
                }
            });
        })
        .trigger(HelpCircle(
            div()
                .id(trigger_id)
                .size(px(14.0))
                .rounded_full()
                .border_1()
                .border_color(border_color)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.0))
                .text_color(muted_foreground)
                .on_hover(move |hovered, _window, cx| {
                    let entity = hover_entity.clone();
                    let key = hover_key.clone();
                    entity.update(cx, |view, cx| {
                        if *hovered {
                            view.hover_help = Some(key.clone());
                        } else if view.hover_help.as_deref() == Some(key.as_str()) {
                            view.hover_help = None;
                        }
                        cx.notify();
                    });
                })
                .child("?"),
        ))
        .content(move |_state, _window, cx| {
            // 自绘提示框，不用 Tooltip 组件：它自带 `.m_3()`（12px margin，
            // tooltip.rs:118），那正是「offset 调成 0 仍有距离」的来源。正文
            // 用 caption 字号 + muted 灰，长文本在 max_w 内换行，不溢出右边框。
            div()
                .max_w(px(420.0))
                .bg(cx.theme().popover)
                .text_color(cx.theme().muted_foreground)
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius)
                .px_2()
                .py_1()
                .text_size(crate::sp(crate::CAPTION_SIZE))
                .child(help.clone())
                .into_any_element()
        })
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
                // ADR-0104 §7: `strict` needs `sandbox-exec` — the only backend
                // with a hard egress. Offering it for landlock/bwrap/… lets a
                // user pick a combination that fails at assembly, which is the
                // very thing the ADR exists to prevent, so it is hidden unless
                // the mode that can honour it is selected.
                let mut options = options.to_vec();
                if (field.path == ["sandbox", "network"] || field.path == ["network", "mode"])
                    && !forms::network_strict_allowed(
                        self.selects
                            .get("sandbox.mode")
                            .map(String::as_str)
                            .unwrap_or_default(),
                    )
                {
                    options.retain(|option| *option != "strict");
                }
                let mut row = div().flex().flex_row().gap_1();
                for option in options.iter() {
                    let marker = if current == *option { "●" } else { "○" };
                    let key = key.clone();
                    let value = option.to_string();
                    let entity = entity.clone();
                    let option = *option;
                    row = row.child(
                        Button::new(format!("select-{key}-{option}"))
                            .label(format!("{marker} {option}"))
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |view, cx| {
                                    // `bwrap` needs bubblewrap installed: picking it
                                    // without the package would fail at assembly
                                    // with a bare error. Say so now, and leave the
                                    // selection untouched until it is available.
                                    if key == "sandbox.mode"
                                        && option == "bwrap"
                                        && !qcode_tools::sandbox::bwrap_installed()
                                    {
                                        view.sandbox_blocked_message =
                                            Some(qcode_i18n::tr("gui.setup.sandbox.bwrap_missing"));
                                        cx.notify();
                                        return;
                                    }
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
                    .aria_label(qcode_i18n::tr(field.label))
                    .into_any_element()
            }
            _ => {
                let Some(input) = self.inputs.get(&key) else {
                    return div().into_any_element();
                };
                Input::new(input)
                    .aria_label(qcode_i18n::tr(field.label))
                    .into_any_element()
            }
        };
        // 全页统一的字段行：标签文字 + 问号圆圈作为文本行内元素流式排布。
        // 普通 div 做不到「圆圈跟在换行标签最后一个字后面」——换行的文本盒
        // 占满整列、圆圈只能贴盒右缘；走 TextView 的 markdown 行内元素管线
        // 后圆圈随文字一起换行，永远停在最后一个字符后面。
        let plugin = HelpInlinePlugin {
            entity: cx.entity(),
            key: key.clone(),
            help: field.help,
        };
        // 标记前留一个空格：圆圈与最后一个字符之间要有间距。
        let markdown = format!("{} <qc-help/>", qcode_i18n::tr(field.label));
        // 不能是 flex 容器：TextView 作为 flex 子项按内容宽度布局、不收缩
        // 换行。普通块级容器会让 TextView 填满标签列宽（行宽的 16%）并换行。
        let label_block = div().w(relative(0.16)).flex_shrink(0.0).child(
            TextView::markdown(format!("label-{key}"), markdown)
                .selectable(false)
                .markdown_extensions(MarkdownExtensions::default().plugin(plugin)),
        );
        let control_block = div().flex_1().min_w_0().child(control);
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap_6()
            .py_1()
            .child(label_block)
            .child(control_block)
            .into_any_element()
    }

    /// A sandbox mode the machine cannot honour (e.g. `bwrap` without
    /// bubblewrap): a modal explains how to make it available, and the
    /// selection stays put. The modal is deliberately a plain overlay like the
    /// delete confirmation — the whole point is that the click changed nothing.
    pub(crate) fn render_sandbox_blocked_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let Some(message) = self.sandbox_blocked_message.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (popover, popover_fg, muted_color, border_color) = (
            theme.popover,
            theme.popover_foreground,
            theme.muted_foreground,
            theme.border,
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
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(popover)
                    .border_1()
                    .border_color(border_color)
                    .p_4()
                    .text_color(popover_fg)
                    .child(qcode_i18n::tr("gui.setup.sandbox.unavailable"))
                    .child(
                        div()
                            .text_size(crate::sp(crate::SECONDARY_SIZE))
                            .text_color(muted_color)
                            .child(message),
                    )
                    .child(
                        div().flex().flex_row().justify_end().gap_2().child(
                            Button::new("close-sandbox-blocked")
                                .label(qcode_i18n::tr("gui.setup.sandbox.dismiss"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.sandbox_blocked_message = None;
                                    cx.notify();
                                })),
                        ),
                    ),
            )
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
                                        .aria_label(qcode_i18n::tr(column.label))
                                        .into_any_element()
                                }
                                _ => {
                                    let Some(input) = row.cells.get(column.key) else {
                                        continue;
                                    };
                                    Input::new(input)
                                        .aria_label(qcode_i18n::tr(column.label))
                                        .into_any_element()
                                }
                            };
                            card = card.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().child(qcode_i18n::tr(column.label)))
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
                                        div().child(qcode_i18n::tr("gui.setup.provider_key_label")),
                                    )
                                    .child(
                                        Input::new(secret).aria_label(qcode_i18n::tr(
                                            "gui.setup.provider_key_aria",
                                        )),
                                    )
                                    .child(
                                        Button::new(format!("store-key-{key}-{key_row_id}"))
                                            .label(qcode_i18n::tr("gui.setup.store_key"))
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
                                .label(qcode_i18n::tr("gui.setup.action.remove"))
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
            .child(div().child(qcode_i18n::tr(table.title)))
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
                    .label(qcode_i18n::tr("gui.setup.add_custom_row"))
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

impl SetupView {
    /// The Appearance page's `采样一次` button (ADR-0064).
    ///
    /// A sample runs the login shell, which takes up to `env_timeout_ms`;
    /// doing that on the UI thread would freeze the window, so it goes to a
    /// worker thread and the summary arrives through a channel. Nothing is
    /// persisted — this only tells the user what `run_shell` would see.
    pub(crate) fn probe_shell_environment(&mut self, cx: &mut Context<Self>) {
        if self.shell_probe_rx.is_some() {
            return;
        }
        let settings = self.config.shell.clone();
        if settings.env_sampling == qcode_tools::shell_env::EnvSampling::Off {
            self.shell_probe_note = Some(qcode_i18n::tr("gui.setup.shell_probe.off").to_string());
            cx.notify();
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let cache = qcode_tools::shell_env::ShellEnvCache::new(settings);
            let sample = cache.current();
            let report = cache.last_report().unwrap_or_default();
            let message = match sample {
                Some(sample) => qcode_i18n::trp(
                    "gui.setup.shell_probe.summary",
                    &[
                        sample.program().display().to_string(),
                        sample.len().to_string(),
                        report.duration_ms.to_string(),
                        if report.cached {
                            qcode_i18n::tr("gui.setup.shell_probe.cached").to_string()
                        } else {
                            String::new()
                        },
                    ],
                ),
                None => qcode_i18n::trp(
                    "gui.setup.shell_probe.failed",
                    &[report.error.unwrap_or_else(|| {
                        qcode_i18n::tr("gui.setup.shell_probe.unknown_cause").to_string()
                    })],
                ),
            };
            let _ = tx.send(message);
        });
        self.shell_probe_rx = Some(rx);
        self.shell_probe_note = Some(qcode_i18n::tr("gui.setup.shell_probe.running").to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let done = this.update(cx, |view, cx| {
                    if view.closing {
                        view.shell_probe_rx = None;
                        return true;
                    }
                    match view
                        .shell_probe_rx
                        .as_ref()
                        .map(std::sync::mpsc::Receiver::try_recv)
                    {
                        Some(Ok(message)) => {
                            view.shell_probe_rx = None;
                            view.shell_probe_note = Some(message);
                            cx.notify();
                            true
                        }
                        Some(Err(std::sync::mpsc::TryRecvError::Empty)) => false,
                        _ => {
                            view.shell_probe_rx = None;
                            view.shell_probe_note = Some(
                                qcode_i18n::tr("gui.setup.shell_probe.interrupted").to_string(),
                            );
                            cx.notify();
                            true
                        }
                    }
                });
                if done.unwrap_or(true) {
                    break;
                }
            }
        })
        .detach();
    }

    /// The button and its one-line result, shown under the `[shell]` fields.
    pub(crate) fn render_shell_environment_probe(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div().flex().flex_row().items_center().gap_2().child(
            Button::new("shell-env-probe")
                .label(qcode_i18n::tr("gui.setup.shell_probe.action"))
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.probe_shell_environment(cx);
                })),
        );
        if let Some(note) = &self.shell_probe_note {
            row = row.child(note.clone());
        }
        row.into_any_element()
    }
}
