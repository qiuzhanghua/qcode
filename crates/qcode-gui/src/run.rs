use crate::*;

/// Open the window and drive it until the user closes it.
pub fn run(app: App, backend: Box<dyn Backend>, config_path: PathBuf, data_dir: PathBuf) {
    let prefs = GuiPreferences::load(&data_dir);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.activate(true);
            prefs.apply(cx);
            // 退出前杀掉 run_shell 还挂着的进程组（ADR-0073）。
            //
            // 必须挂在这里、而且是同步的：`cx.quit()` 之后进程直接结束，引擎
            // 那个 detached 线程不会展开栈，所以 `kill_on_drop` 一次都不会跑
            // —— 一个卡在网络上、既不写输出也等不到 stdin EOF 的命令就会活过
            // qcode。`on_app_quit` 覆盖所有退出路径（窗口 ✕ 不是退出，见
            // ADR-0052；托盘退出、设置窗口退出都经过这里），而它的预算是
            // `gpui::SHUTDOWN_TIMEOUT` = 200ms，一串 `kill(2)` 是微秒级。
            let quit_hook = cx.on_app_quit(|_| {
                let killed = qcode_tools::child_groups::kill_all();
                if killed > 0 {
                    // Safe to log at quit time: the file writer is synchronous, so
                    // this lands before the process ends rather than being dropped
                    // with an async queue.
                    tracing::info!(killed, "killed leftover process groups on quit");
                }
                async {}
            });
            // Deliberately never dropped: the returned `Subscription` is what
            // keeps the observer registered, so letting it fall out of scope
            // here would unregister the hook immediately and leave the cleanup
            // silently dead. It is meant to live exactly as long as the process,
            // which is what leaking it expresses; there is nothing to unbind it
            // for.
            std::mem::forget(quit_hook);
            let (tray, tray_rx) = tray::start();
            let minimize_on_close = tray.is_some();
            let window_options = window_icon::attach(match prefs.window_geometry.as_ref() {
                Some(geometry) => WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(geometry.x), px(geometry.y)),
                        size(px(geometry.width), px(geometry.height)),
                    ))),
                    ..Default::default()
                },
                // First run: no saved geometry yet, so open at the desktop
                // default size, centred, instead of the platform default.
                None => WindowOptions {
                    window_bounds: Some(WindowBounds::centered(
                        size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT)),
                        cx,
                    )),
                    ..Default::default()
                },
            });
            let zoom = zoom_keystrokes(cx.platform_info().is_mac());
            cx.bind_keys([
                // Ctrl+V stays the composer's text paste; image paste is
                // Ctrl+Shift+V so the two never collide.
                KeyBinding::new("ctrl-shift-v", PasteImage, None),
                KeyBinding::new("cmd-shift-v", PasteImage, None),
                // Same reason as Cmd+Enter below: the focused Textarea owns
                // "Input", so the root binding never fires while typing.
                KeyBinding::new("ctrl-shift-v", PasteImage, Some("Input")),
                KeyBinding::new("cmd-shift-v", PasteImage, Some("Input")),
                KeyBinding::new("y", ApproveOnce, Some("Approval")),
                KeyBinding::new("a", ApproveSession, Some("Approval")),
                KeyBinding::new("n", RejectApproval, Some("Approval")),
                KeyBinding::new("cmd-n", NewSessionShortcut, None),
                KeyBinding::new("cmd-,", OpenSettingsShortcut, None),
                KeyBinding::new("cmd-w", CloseWindowShortcut, None),
                KeyBinding::new("cmd-b", ToggleSidebarShortcut, None),
                KeyBinding::new("cmd-shift-t", ToggleThemeShortcut, None),
                // Cmd+= / Cmd+- on macOS, Ctrl+= / Ctrl+- elsewhere. Zoom in
                // is bound both unshifted and shifted so it works however the
                // layout reports Cmd/Ctrl+Shift+=.
                KeyBinding::new(zoom.zoom_in, ZoomInShortcut, None),
                KeyBinding::new(zoom.zoom_in_shifted, ZoomInShortcut, None),
                KeyBinding::new(zoom.zoom_out, ZoomOutShortcut, None),
                KeyBinding::new("cmd-.", StopTurnShortcut, None),
                KeyBinding::new("cmd-alt-up", PrevTurnShortcut, None),
                KeyBinding::new("cmd-alt-down", NextTurnShortcut, None),
                // Cmd+T toggles the terminal view: in, and back out again.
                KeyBinding::new("cmd-t", OpenTerminalShortcut, None),
                // Inside the terminal, Cmd+W closes the tab, not the window: the
                // context-scoped binding wins over the root one.
                KeyBinding::new("cmd-w", CloseTerminalShortcut, Some("Terminal")),
                // Escape closes 设置. Scoped to the settings window's own key context:
                // unbindable otherwise, since every other Escape in the app belongs to
                // whatever has focus (a text field cancels its IME with it).
                KeyBinding::new("escape", CloseSettingsShortcut, Some("Settings")),
                // Only inside the terminal: everywhere else Cmd+C belongs to the
                // text fields and the transcript.
                KeyBinding::new("cmd-c", CopySelectionShortcut, Some("Terminal")),
                KeyBinding::new("ctrl-shift-c", CopySelectionShortcut, Some("Terminal")),
                KeyBinding::new("ctrl-tab", NextTerminalShortcut, Some("Terminal")),
                KeyBinding::new("ctrl-shift-tab", PrevTerminalShortcut, Some("Terminal")),
                KeyBinding::new("cmd-enter", SendMessage, Some("Chat")),
                // The focused Textarea owns the "Input" context, so bind there too
                // or the root handler never sees Cmd+Enter.
                KeyBinding::new("cmd-enter", SendMessage, Some("Input")),
                // Provider/Model 菜单的键盘选择（弹层内容挂 "ModelMenu"
                // key_context）：↑↓ 移动高亮、Enter 选中。
                KeyBinding::new("up", gpui_kit::base::actions::SelectUp, Some("ModelMenu")),
                KeyBinding::new(
                    "down",
                    gpui_kit::base::actions::SelectDown,
                    Some("ModelMenu"),
                ),
                KeyBinding::new(
                    "enter",
                    gpui_kit::base::actions::Confirm { secondary: false },
                    Some("ModelMenu"),
                ),
            ]);
            // Global fallback for the zoom shortcuts: window-level handlers
            // (Shell, settings SetupView) normally consume these; this runs
            // when no window handler did (e.g. a settings window focused but
            // its handler did not fire), so Ctrl+= / Ctrl+- always zoom.
            let zoom_data_dir = data_dir.clone();
            cx.on_action(move |_: &ZoomInShortcut, cx| {
                crate::zoom_font_global(1, &zoom_data_dir, cx);
            });
            let zoom_data_dir = data_dir.clone();
            cx.on_action(move |_: &ZoomOutShortcut, cx| {
                crate::zoom_font_global(-1, &zoom_data_dir, cx);
            });
            let (window_handle, shell) =
                gpui_kit::open_window(window_options, cx, move |window, cx| {
                    window_icon::identify(window);
                    window.activate_window();
                    if minimize_on_close {
                        // 关闭按钮 = 隐藏到托盘（ADR-0052）；真正退出走托盘/菜单。
                        // 这里持有 `&mut App`，必须用延迟版本，否则 AppKit 在
                        // `orderOut:` 里同步派发的帧请求会二次借用 App。
                        window.on_window_should_close(cx, |window, cx| {
                            tray::hide_window_later(window, cx);
                            false
                        });
                    }
                    cx.new(|cx| {
                        Shell::new(
                            app,
                            backend,
                            config_path,
                            data_dir,
                            window,
                            cx,
                            minimize_on_close,
                        )
                    })
                })
                .expect("failed to open the window");

            // ADR-0058 §5: the engine's events are drained by a resident loop,
            // not by `render`. Started here because the entity exists now, and
            // because a minimized window never reaches a render.
            shell.update(cx, |shell, cx| shell.start_poll_ticker(cx));

            if let Some(tray) = tray {
                let tray_rx = tray_rx;
                cx.spawn(async move |cx| {
                    let mut last: Option<tray::TraySnapshot> = None;
                    loop {
                        while let Ok(command) = tray_rx.try_recv() {
                            match command {
                                tray::TrayCommand::ShowWindow => {
                                    let _ = window_handle.update(cx, |_view, window, cx| {
                                        // 这里持有 `&mut App`，原生还原/置顶必须
                                        // 推迟，否则激活事件同步请求帧会二次借用。
                                        tray::show_window_later(window, cx);
                                    });
                                }
                                tray::TrayCommand::NewTask => {
                                    // 不能走 dispatch_action：窗口最小化时没有焦点，
                                    // 动作派发不到根节点。
                                    let _ = window_handle.update(cx, |_view, window, cx| {
                                        shell.update(cx, |shell, cx| shell.new_session(window, cx))
                                    });
                                }
                                tray::TrayCommand::StopCurrent => {
                                    let _ = window_handle.update(cx, |_view, _window, cx| {
                                        shell.update(cx, |shell, cx| {
                                            shell.stop();
                                            cx.notify();
                                        })
                                    });
                                }
                                tray::TrayCommand::Quit => {
                                    let _ = window_handle.update(cx, |_view, window, cx| {
                                        shell.update(cx, |shell, cx| shell.request_quit(window, cx))
                                    });
                                }
                            }
                        }
                        let snapshot = cx.update(|cx| shell.read(cx).tray_snapshot());
                        if last.as_ref() != Some(&snapshot) {
                            tray.update(snapshot.clone());
                            last = Some(snapshot);
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(200))
                            .await;
                    }
                })
                .detach();
            }
        });
}
