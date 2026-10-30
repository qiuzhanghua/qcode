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
            let (tray, tray_rx) = tray::start();
            let minimize_on_close = tray.is_some();
            let window_options = match prefs.window_geometry.as_ref() {
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
            };
            let zoom = zoom_keystrokes(cx.platform_info().is_mac());
            cx.bind_keys([
                // Ctrl+V stays the composer's text paste; image paste is
                // Ctrl+Shift+V so the two never collide.
                KeyBinding::new("ctrl-shift-v", PasteImage, None),
                KeyBinding::new("cmd-shift-v", PasteImage, None),
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
                KeyBinding::new("cmd-enter", SendMessage, Some("Chat")),
                // The focused Textarea owns the "Input" context, so bind there too
                // or the root handler never sees Cmd+Enter.
                KeyBinding::new("cmd-enter", SendMessage, Some("Input")),
            ]);
            let (window_handle, shell) =
                gpui_kit::open_window(window_options, cx, move |window, cx| {
                    window.activate_window();
                    if minimize_on_close {
                        // 关闭按钮 = 隐藏到托盘（ADR-0052）；真正退出走托盘/菜单。
                        window.on_window_should_close(cx, |window, _cx| {
                            tray::hide_window(window);
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

            if let Some(tray) = tray {
                let tray_rx = tray_rx;
                cx.spawn(async move |cx| {
                    let mut last: Option<tray::TraySnapshot> = None;
                    loop {
                        while let Ok(command) = tray_rx.try_recv() {
                            match command {
                                tray::TrayCommand::ShowWindow => {
                                    let _ = window_handle.update(cx, |_view, window, _cx| {
                                        tray::show_window(window);
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
