use crate::*;

/// 激活请求 → 托盘「显示主窗口」。
///
/// 单实例模块（`qcode_core::single_instance`）收到 `activate` 后只把请求交给
/// 调用方，具体做什么由这里定。走托盘那条队列是有意的（ADR-0084 §3）：托盘
/// 「显示主窗口」已经处理了三端各自「原生调用必须推迟到 foreground executor」
/// 的坑（macOS 还要 `deminiaturize:`），另写一条「把窗口弄出来」的路只会有
/// 第二份平台细节和第二个 bug。
///
/// 返回 `false` = 队列已经断了，监听线程该收摊。
/// 激活通道的回调。
///
/// GUI 只认 `Activate`。`Stop` 直接忽略并继续接活——理由不是「还没做」，而是
/// **窗口不该被任何外部渠道关掉**：用户可能正在里面看着一次回合。而且实际上也
/// 收不到：`qc daemon stop` 连的是 `Scope::QCD` 的通道，与 GUI 的 `gui.sock`
/// 是两个文件（ADR-0092 §5 与 ADR-0083 各一把锁）。
fn activator(
    tx: std::sync::mpsc::Sender<tray::TrayCommand>,
) -> impl Fn(qcode_core::single_instance::Request, &mut dyn std::io::Write) -> bool + Send + 'static
{
    use qcode_core::single_instance::{Request, ack};
    move |request, reply| match request {
        Request::Activate => {
            // 应答只在真的把请求塞进托盘队列之后写：队列断了说明窗口已经不在了，
            // 那时候回 `ok` 会让调用者以为它被浮起来了。
            let sent = tx.send(tray::TrayCommand::ShowWindow).is_ok();
            if sent {
                let _ = ack(request, reply);
            }
            sent
        }
        // 窗口不接受外部关闭。不回 `ok` 也不退出——「我听到了但我不干」这件事
        // 协议里没有第三种回复，而沉默配合 `true` 正是它的表达。
        Request::Stop => {
            tracing::warn!("a stop request reached the GUI channel; ignoring it");
            true
        }
    }
}

/// The largest window with a comfortable 16:9 aspect that fits 90% of the
/// work area, clamped into the usable band.
///
/// The ratio is fixed so the window keeps one shape on every display; the
/// independent width/height fractions it replaces let the aspect drift with
/// the screen (1.6 on ultrawide up to 2.2 on 4K). The floors may bend the
/// ratio on very small screens — a usable minimum beats a strict shape.
fn first_run_size(work: Size<Pixels>) -> Size<Pixels> {
    const RATIO: f32 = 16.0 / 9.0;
    let max_w = (work.width.as_f32() * 0.9).min(MAX_WINDOW_WIDTH);
    let max_h = (work.height.as_f32() * 0.9).min(MAX_WINDOW_HEIGHT);
    let (w, h) = if max_w / max_h > RATIO {
        // The screen is wider than 16:9, so the window is height-limited.
        (max_h * RATIO, max_h)
    } else {
        // The screen is taller/narrower, so the window is width-limited.
        (max_w, max_w / RATIO)
    };
    let w = w.max(MIN_WINDOW_WIDTH);
    let h = h.max(MIN_WINDOW_HEIGHT);
    size(
        px(w.min(work.width.as_f32())),
        px(h.min(work.height.as_f32())),
    )
}

/// Open the window and drive it until the user closes it.
///
/// `single` 是本进程抢到的那把单实例锁（ADR-0083）。它必须一路传到这里：
/// 激活通道要把请求送进托盘的命令队列，而那条队列在窗口建好之后才存在。
/// `None` 表示互斥被关掉了（`QC_GUI_SINGLE_INSTANCE=0` 或锁机制不可用）。
pub fn run(
    app: App,
    backend: Box<dyn Backend>,
    config_path: PathBuf,
    data_dir: PathBuf,
    single: Option<qcode_core::single_instance::Primary>,
) {
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
                // The caller's exit bookkeeping. On macOS this is the *only*
                // chance: `-[NSApplication terminate:]` calls `exit()` from AppKit
                // after this observer runs, so `main` never resumes and anything
                // written from its return path is lost (see the `quit` module).
                crate::quit::fire();
                async {}
            });
            // Deliberately never dropped: the returned `Subscription` is what
            // keeps the observer registered, so letting it fall out of scope
            // here would unregister the hook immediately and leave the cleanup
            // silently dead. It is meant to live exactly as long as the process,
            // which is what leaking it expresses; there is nothing to unbind it
            // for.
            std::mem::forget(quit_hook);
            let (tray, tray_rx, tray_tx) = tray::start();
            let minimize_on_close = tray.is_some();
            // 托管激活通道。放在这里而不是 `cmd_gui`：它是把请求塞进托盘队列
            // 的那一端，没有队列可塞就没有意义。`serve` 会把锁句柄移进监听
            // 线程，于是锁活到进程结束（ADR-0083 §1：不做「退出时解锁」）。
            let activated = single.is_some();
            if let Some(single) = single {
                single.serve(activator(tray_tx));
            }
            let window_options = window_icon::attach(match prefs.window_geometry.as_ref() {
                Some(geometry) => WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(geometry.x), px(geometry.y)),
                        size(px(geometry.width), px(geometry.height)),
                    ))),
                    ..Default::default()
                },
                // First run: no saved geometry yet, so size the window to the
                // screen and centre it instead of the platform default. The
                // size is the largest 16:9 window that fits 90% of the work
                // area, clamped into the usable band — every screen ends up
                // with the same comfortable shape instead of a ratio that
                // drifts with the display.
                None => {
                    let size = match cx.primary_display() {
                        Some(display) => first_run_size(display.visible_bounds().size),
                        None => size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT)),
                    };
                    WindowOptions {
                        window_bounds: Some(WindowBounds::centered(size, cx)),
                        ..Default::default()
                    }
                }
            });
            let zoom = zoom_keystrokes(cx.platform_info().is_mac());
            // Image paste is Ctrl+Shift+V (Cmd+Shift+V on a mac keyboard) rather
            // than Ctrl+V so the two never collide. It is bound twice per stroke:
            // context-free for the window, and "Input" because the focused
            // Textarea owns that context, so the root binding never fires while
            // typing.
            //
            // The terminal rebinds both strokes at its own context depth below —
            // a binding is dispatched before the terminal's key-down handler, so
            // without that the image paste would shadow the terminal's paste.
            for key in image_paste_keystrokes() {
                cx.bind_keys([
                    KeyBinding::new(key, PasteImage, None),
                    KeyBinding::new(key, PasteImage, Some("Input")),
                ]);
            }
            // ...and the terminal takes them back at its own context depth. A
            // binding is dispatched before `on_key_down`, so the terminal's
            // key-down paste (`is_paste`) never sees a stroke the window binds:
            // without this, Ctrl+Shift+V in the terminal is an image paste, and
            // off macOS there is no paste key left at all (plain Ctrl+V is the
            // program's byte). Deeper context wins. ADR-0062 §5.
            for key in terminal_paste_keystrokes() {
                cx.bind_keys([KeyBinding::new(key, PasteIntoTerminal, Some("Terminal"))]);
            }
            cx.bind_keys([
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

            // 没有托盘时这条循环本来不必存在（关窗即退出，`minimize_on_close`
            // 为假）。但单实例激活（ADR-0084）要有人把请求变成「把窗口拿到
            // 前台」，而那是同一条队列——所以只要有 `single` 就得跑。没有托盘
            // 的那一支只是跳过 `update`，行为与从前一致。
            if tray.is_some() || activated {
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
                                tray::TrayCommand::StopCurrent => {
                                    // 托盘不再有「新建任务」：关窗 = 真隐藏
                                    // （`tray::hide_window_later`），隐藏时没有键盘
                                    // 焦点，`cmd-n` / 段头 `+` 都按不到，所以那一路
                                    // 只剩「显示主窗口」这一个前提。停止留着——它是
                                    // 窗口不可见时唯一的刹车，删掉等于把托盘降级成
                                    // 纯状态显示。
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
                        if let Some(tray) = &tray {
                            let snapshot = cx.update(|cx| shell.read(cx).tray_snapshot());
                            if last.as_ref() != Some(&snapshot) {
                                tray.update(snapshot.clone());
                                last = Some(snapshot);
                            }
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

#[cfg(test)]
mod tests {
    // 刻意不写 `use super::*`：`run.rs` 顶部是 `use crate::*`，在这里再 glob
    // 一次会让名字解析把整个 crate 根重新过一遍，`gpui_kit::actions!` 那次
    // 展开当场把 rustc 打崩（SIGSEGV，只在 `cargo test` 这个 target 上复现）。
    use super::activator;
    use crate::tray;

    /// ADR-0084 §3：激活请求必须落在托盘那条「显示主窗口」上。
    ///
    /// 单实例模块现在只负责「把请求交给调用方」（它住在 `qcode_core`，不认识
    /// 任何界面类型），所以这条映射要在这里单独钉住——两段各自被自己的测试
    /// 覆盖，合起来才是原来那条从 socket 到 `ShowWindow` 的链。
    #[test]
    fn an_activate_request_becomes_a_show_window_command() {
        use qcode_core::single_instance::Request;

        let (tx, rx) = std::sync::mpsc::channel();
        let handler = activator(tx);
        // 应答写入端在真通道里是客户端那条连接；这里用一个 `Vec` 顶替，
        // 顺便能断言「真的回了 ok」。
        let mut reply: Vec<u8> = Vec::new();
        assert!(
            handler(Request::Activate, &mut reply),
            "队列还在时必须说「继续」"
        );
        assert_eq!(rx.try_recv(), Ok(tray::TrayCommand::ShowWindow));
        assert_eq!(
            String::from_utf8_lossy(&reply),
            "{\"ok\":true}\n",
            "接到队列之后必须回应答，否则调用者以为没送到"
        );

        // 接收端没了：监听线程该收摊，而不是继续接受无人认领的请求。
        // 而且**不能**回 `ok`——那会让调用者以为窗口被浮起来了。
        drop(rx);
        let mut reply: Vec<u8> = Vec::new();
        assert!(
            !handler(Request::Activate, &mut reply),
            "队列断了之后必须说「停」"
        );
        assert!(
            reply.is_empty(),
            "队列断了就不该回应答，got {:?}",
            String::from_utf8_lossy(&reply)
        );
    }

    /// 窗口不接受外部关闭。
    ///
    /// `qc daemon stop` 连的是 `Scope::QCD` 的通道，与 GUI 那条是两个文件，所以
    /// 正常情况下这个回调收不到 `Stop`。但「收不到」是靠作用域保证的，不是靠这里
    /// ——所以这条钉住「万一收到了也别退出」。
    #[test]
    fn a_stop_request_does_not_close_the_window() {
        use qcode_core::single_instance::Request;

        let (tx, _rx) = std::sync::mpsc::channel();
        let handler = activator(tx);
        let mut reply: Vec<u8> = Vec::new();
        assert!(
            handler(Request::Stop, &mut reply),
            "窗口必须继续接活，而不是被外部关掉"
        );
        assert!(
            reply.is_empty(),
            "不回 ok：沉默 + 继续，才是「我听到了但我不干」"
        );
    }
}
