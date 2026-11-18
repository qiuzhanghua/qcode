//! 定时任务段头那个乒乓按钮的**判断**（ADR-0098 §2）。
//!
//! 这里只有两件事：
//!
//! 1. 把 `core::daemon::Watch` 的两个粒度翻成一个 [`DaemonButton`]——界面不自己
//!    推判据，它只读这张表。
//! 2. 点下去之后做什么（`start` / `stop`），以及**过渡态**：`stop` 要等正在收尾的
//!    回合，而用户看不到任何变化时会连点。
//!
//! # 为什么判断要单独一个类型
//!
//! ADR-0098 §3 那张表有四格，每格三种产物（标签、能不能点、说明行）。写成一个
//! `match` 是四段各写三样东西，总共十二条字面量摊在渲染函数里——而那十二条正是
//! 用户判断「我现在会不会收到提醒」的全部依据。抽出来之后：
//!
//! - 状态机有测试（`the_four_cells_of_the_button`）；
//! - 渲染只负责画笔与配色；
//! - 改动只会发生在这一处。

use qcode_core::daemon::{Presence, Watch};
use qcode_core::scheduler::{DriverStatus, driver_footer};

use super::*;

/// 按钮上的过渡态：请求发出去了，状态还没跟上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonTransition {
    Starting,
    Stopping,
}

/// 段头那个按钮此刻该长什么样。
///
/// 四种，对应 ADR-0098 §3 的表；不多不少——**第五种就是「设计漏了一格」**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DaemonButton {
    /// 没有 `qcd`，本目录也没人推时钟：点它起一个 daemon。
    Start,
    /// 没有 `qcd`，但本目录有驱动（**窗口自己**）。不可点。
    ///
    /// 这一格是整张表里最需要解释的：如果显示「启动」，点了会撞上 ADR-0092 §4 那句
    /// 「waiting for it to exit」——一个点了不动的按钮。用户真正需要知道的是
    /// 「关窗就停」。
    WindowDriven,
    /// `qcd` 在跑，且本目录的时钟就是它在推：点它停掉。
    Stop,
    /// `qcd` 在跑，但服务的是**另一个**数据目录。不可点。
    ///
    /// 不报具体路径：锁只回答「有人」（见 `core::daemon::Watch` 的文档与
    /// `qc daemon status` 的说法）。宁可说「另一个目录」，也不要猜一个。
    OccupiedElsewhere,
    /// 判不了（锁机制坏了，或后端还没装配）。
    Unknown,
    /// 过渡态：`start` / `stop` 在途中。
    Busy(DaemonTransition),
}

impl DaemonButton {
    /// **`qcd` 这个进程在不在跑。** 段头那颗图标绿不绿，只看这一件事。
    ///
    /// 与 [`DriverTone`] 是两个问题，别混：那是「这些任务到点会不会自己响」
    /// （数据目录级），这是「那个常驻进程在不在」（每用户一个）。`WindowDriven`
    /// 正是两者不一致的那一格——窗口在推时钟（段落不灰），但没有 `qcd`（图标灰）。
    ///
    /// 过渡态按**此刻的事实**算，不按「将要怎样」算：`Stopping` 期间它还在（正在
    /// 收尾），`Starting` 期间它还没起来。这一段只有一两秒，但把它画反了就等于
    /// 告诉用户「已经没了」——而那时候 `qc daemon status` 还会说它在。
    pub(crate) fn qcd_running(&self) -> bool {
        matches!(
            self,
            Self::Stop | Self::OccupiedElsewhere | Self::Busy(DaemonTransition::Stopping)
        )
    }

    /// 能点吗。**只有过渡态不能点**（ADR-0100）。
    ///
    /// 旧版只让 Start / Stop 可点，其余四格是灰按钮加一句「为什么不能点」。那是
    /// 错的设计：`WindowDriven` 恰恰是最该能点的一格——用户开着窗口，想要「关掉
    /// 窗口后还在跑」，正确动作就是**现在**起一个 daemon，让它等窗口退出后接手
    /// （ADR-0092 §10 让它等得起）。做成死按钮等于把用户逼到「先关窗口，再去启动」
    /// ——而窗口一关，这颗按钮他也看不见了。
    pub(crate) fn actionable(&self) -> bool {
        !matches!(self, Self::Busy(_))
    }

    /// 悬停提示：说清**点下去会发生什么**，以及图标为什么是那个亮度。
    pub(crate) fn tooltip(&self) -> &'static str {
        match self {
            Self::Start => "常驻驱动没在跑（图标灰）。点一下启动：关掉窗口后任务仍会触发。",
            Self::Stop => "常驻驱动在跑（图标绿）。点一下停止：到点不再触发，任务与列表都还在。",
            // 这一格最需要解释：窗口在推、但没有 daemon。点了会起一个**排队**的
            // daemon（ADR-0092 §10），不是抢。
            Self::WindowDriven => {
                "现在由这个窗口驱动，没有常驻驱动（图标灰）。\
                 点一下启动：它会等这个窗口退出后接手。"
            }
            Self::OccupiedElsewhere => {
                "常驻驱动在跑（图标绿），但服务的是另一个数据目录——一个用户只有一个。\
                 点一下把它停掉。"
            }
            Self::Unknown => "查不出驱动状态（图标灰）。点一下会尝试启动。",
            Self::Busy(DaemonTransition::Starting) => "正在启动…",
            Self::Busy(DaemonTransition::Stopping) => "正在停止…",
        }
    }

    /// 状态的一句话说清（`/daemon status` 与无障碍标签共用）。
    ///
    /// 不再是「画在图标下面的字」——那颗按钮现在只有一个图标（ADR-0100）。文字仍
    /// 然要有：一个只有图形的按钮，屏幕阅读器读不出任何东西。
    pub(crate) fn state_word(&self) -> &'static str {
        match self {
            Self::Start => "未常驻",
            Self::Stop => "常驻中",
            Self::WindowDriven => "仅窗口驱动",
            Self::OccupiedElsewhere => "常驻在别处",
            Self::Unknown => "状态未知",
            Self::Busy(DaemonTransition::Starting) => "启动中",
            Self::Busy(DaemonTransition::Stopping) => "停止中",
        }
    }
}

/// 段头该不该把定时任务画成灰的（ADR-0098 §4）。
///
/// **灰 = 「不会自己发生」**，不是「你不能动手」。所以它只影响颜色：
/// 列表项照样能点开、能编辑、能删（ADR-0090 的全部前提）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriverTone {
    /// 正常：到点会触发。
    Live,
    /// 黄字警告：在跑但睡着了（`Held`）。**不变灰**——它醒来会继续，而把「被挂起」
    /// 说成「不在跑」就是劝人去重建一条本来没事的任务（ADR-0091）。
    Drowsy,
    /// 灰：驱动不在跑，任务到点不会触发。
    Dormant,
}

/// 段头状态行的一句话。**复用 `driver_footer`**，不另写一套：两处各写一份，迟早
/// 有一份说「会触发」另一份说「不会」，而用户只会信他先看到的那份。
pub(crate) fn status_sentence(watch: Option<&Watch>) -> String {
    let Some(watch) = watch else {
        return "驱动状态未知（后端未装配）。任务仍可登记与编辑，但不会触发。".to_string();
    };
    match &watch.daemon {
        // 本目录没人在推，但这个用户有 daemon 在别处：说清楚「为什么这里不响」，
        // 而不是含糊地报一句「有的」。
        Presence::Running if !watch.will_fire() => {
            "驱动的跑在别的数据目录上；这些任务到点不会触发。".to_string()
        }
        _ => driver_footer(watch.driver),
    }
}

/// 从 `Watch` 与过渡态推按钮该长什么样——**Shell 与测试走同一条路**。
pub(crate) fn decide(watch: Option<&Watch>, transition: Option<DaemonTransition>) -> DaemonButton {
    if let Some(transition) = transition {
        return DaemonButton::Busy(transition);
    }
    let Some(watch) = watch else {
        return DaemonButton::Unknown;
    };
    match (&watch.daemon, watch.will_fire()) {
        // 只在第三/第四条：daemon 在跑，且本目录确实有人推时钟 ⇒ 那个时钟只能是它。
        (Presence::Running, true) => DaemonButton::Stop,
        (Presence::Running, false) => DaemonButton::OccupiedElsewhere,
        // 没有 daemon，但本目录有驱动 ⇒ 窗口自己。显示「启动」是个点了不动的
        // 按钮，所以单独一格。
        (Presence::Absent, true) => DaemonButton::WindowDriven,
        (Presence::Absent, false) => DaemonButton::Start,
        (Presence::Unknown(_), _) => DaemonButton::Unknown,
    }
}

/// 定时任务段与列表的颜色类别。
pub(crate) fn tone(watch: Option<&Watch>) -> DriverTone {
    let Some(watch) = watch else {
        return DriverTone::Dormant;
    };
    match watch.driver {
        DriverStatus::Running { .. } => DriverTone::Live,
        // 在跑、但心跳过期：警告而不是灰。
        DriverStatus::Held { .. } => DriverTone::Drowsy,
        DriverStatus::Gone { .. } | DriverStatus::Unknown => DriverTone::Dormant,
    }
}

impl Shell {
    /// 从缓存的 `Watch` 推按钮该长什么样。
    pub(crate) fn daemon_button(&self) -> DaemonButton {
        decide(self.driver_watch.as_ref(), self.daemon_transition)
    }

    /// 定时任务段与列表的颜色类别。
    pub(crate) fn driver_tone(&self) -> DriverTone {
        tone(self.driver_watch.as_ref())
    }

    /// 刷新驱动状态，按秒节流。
    ///
    /// 一次探测是一次 `flock` 加一次 `runtime_status` 查询——都不贵，但 render 每帧
    /// 问一次就会变成每帧的文件系统调用。一秒足够：用户感知不到，而心跳预算是 90 秒。
    pub(crate) fn refresh_driver_watch(&mut self) {
        let due = self
            .driver_watch_at
            .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(1));
        if !due {
            return;
        }
        self.driver_watch_at = Some(std::time::Instant::now());
        self.driver_watch = self.backend.driver_watch();
        // 过渡态在状态跟上之后就撤销：`start` 等到「有 qcd」，`stop` 等到「没有」。
        // 不靠超时——真正卡住时由下面的 `cancel_stale_transition` 兜底。
        if let (Some(transition), Some(watch)) =
            (self.daemon_transition, self.driver_watch.as_ref())
        {
            let arrived = match transition {
                DaemonTransition::Starting => watch.daemon.is_running(),
                DaemonTransition::Stopping => !watch.daemon.is_running(),
            };
            if arrived {
                self.daemon_transition = None;
            }
        }
        // 兜底（`cancel_stale_transition`）：进程 spawn 成功但状态始终没跟上——比如
        // 刚起就崩在握住用户级锁之前。不能让按钮永远转圈（ADR-0098 §4 没有这一格）。
        if self.daemon_transition.is_some()
            && self
                .daemon_transition_at
                .is_some_and(|at| at.elapsed() >= std::time::Duration::from_secs(10))
        {
            self.daemon_transition = None;
        }
    }

    /// 点段头那颗图标（ADR-0100：**乒乓键**）。
    ///
    /// 判断只剩一个问题——**`qcd` 在跑吗**：在跑就停，没在跑就启。那四格仍然存在，
    /// 但它们现在只决定提示语与「点了会发生什么」，不再决定「能不能点」。
    ///
    /// 两处细节值得留住：
    ///
    /// - `WindowDriven` 点下去是**起一个排队的 daemon**，不是「什么都不做」。它会在
    ///   窗口退出后接手（ADR-0092 §10），而且因为 qcd 在拿驱动锁**之前**就握住了
    ///   用户级锁，下一次探测会如实显示「常驻中」——不会出现「点了但看起来没动」。
    /// - `OccupiedElsewhere` 点下去停的是**这个用户的** daemon，它可能正服务另一个
    ///   数据目录。这与 `qc daemon stop` 是同一个口径（ADR-0098 §2：一个用户一个），
    ///   提示语里写明了。
    pub(crate) fn toggle_daemon(&mut self, cx: &mut Context<Self>) {
        match self.daemon_button() {
            DaemonButton::Stop | DaemonButton::OccupiedElsewhere => self.request_daemon_stop(cx),
            DaemonButton::Start | DaemonButton::WindowDriven | DaemonButton::Unknown => {
                self.request_daemon_start(cx)
            }
            DaemonButton::Busy(_) => {
                self.ui.app.push_notice("正在切换，等它完成。".to_string());
                cx.notify();
            }
        }
    }

    /// `/daemon start|stop|status`：与段头那颗按钮**同一条路**（ADR-0098 §1）。
    ///
    /// 命令里的 `start` / `stop` 不检查按钮是否可点：按钮那四格是**显示**的判断
    /// （「这一格能不能点」），而命令行是用户明确说的意思。撞上「已经有」或
    /// 「窗口自己就是驱动」时，它应该说清结果，而不是假装什么都没发生。
    pub(crate) fn daemon_command(
        &mut self,
        verb: qcode_ui_state::DaemonVerb,
        cx: &mut Context<Self>,
    ) {
        match verb {
            qcode_ui_state::DaemonVerb::Status => {
                // 先重探一次：用户刚打完命令，缓存可能已经过期一秒以上。
                self.driver_watch_at = None;
                self.refresh_driver_watch();
                let sentence = status_sentence(self.driver_watch.as_ref());
                let button = self.daemon_button();
                self.ui
                    .app
                    .push_notice(format!("{} · {sentence}", button.state_word()));
                cx.notify();
            }
            qcode_ui_state::DaemonVerb::Start => self.request_daemon_start(cx),
            qcode_ui_state::DaemonVerb::Stop => self.request_daemon_stop(cx),
        }
    }

    pub(crate) fn request_daemon_start(&mut self, cx: &mut Context<Self>) {
        // 上一个切换还没收尾：按钮已经转圈了，命令行再发一个会撞上「锁在、通道没
        // 人答」。跟 `toggle_daemon` 的 Busy 分支同一句。
        if self.daemon_transition.is_some() {
            self.ui.app.push_notice("正在切换，等它完成。".to_string());
            cx.notify();
            return;
        }
        // 已经在跑就别再起一个：锁会拦住第二个 qcd，但它会**排队等**（ADR-0092 §10），
        // 于是一次误点换来一个永远睡着的进程。这里先问一句，比事后解释便宜。
        if self
            .driver_watch
            .as_ref()
            .is_some_and(|watch| watch.daemon.is_running())
        {
            self.ui
                .app
                .push_notice("常驻驱动已经在跑了，不用再启动。".to_string());
            cx.notify();
            return;
        }
        self.daemon_transition = Some(DaemonTransition::Starting);
        self.daemon_transition_at = Some(std::time::Instant::now());
        let data_dir = self.data_dir.clone();
        // 起进程会阻塞（找二进制、fork、等锁）：放到后台执行器，界面这一帧照画。
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { qcode_core::daemon::start(&data_dir, None) })
                .await;
            this.update(cx, |shell, cx| {
                // 过渡态**不**在这里撤销：`spawn` 返回只说明进程创建成功，qcd 还要
                // 一两秒才握住用户级锁，此刻清掉会露出「灰 → 等 → 绿」的夹缝，看着
                // 像点击没生效。由 `refresh_driver_watch` 在状态跟上（`is_running`）
                // 时撤销；失败或卡住才在这里清。
                // 无论成败都立刻重探一次。
                shell.driver_watch_at = None;
                shell.refresh_driver_watch();
                match result {
                    Ok(pid) => {
                        // 起来了 ≠ 在推时钟。走 `WindowDriven` 那条路时它会在驱动锁上
                        // 排队，等到窗口退出才接手（ADR-0092 §10）——那种「点了但看起来
                        // 没动」必须说出来，否则用户会再点一次。刚 spawn 完可能还在
                        // 就绪，两种「还没在推」分开说。
                        let message = match shell.driver_watch.as_ref() {
                            Some(watch)
                                if watch.daemon.is_running()
                                    && matches!(watch.driver, DriverStatus::Running { .. }) =>
                            {
                                format!("定时任务驱动已启动（pid {pid}）")
                            }
                            Some(watch) if watch.daemon.is_running() => format!(
                                "定时任务驱动已启动（pid {pid}），正在等这个窗口退出后接手。"
                            ),
                            _ => format!("定时任务驱动已启动（pid {pid}），正在就绪…"),
                        };
                        shell.ui.app.push_notice(message);
                    }
                    Err(error) => {
                        shell.daemon_transition = None;
                        shell
                            .ui
                            .app
                            .push_notice(format!("无法启动定时任务驱动：{error}"))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn request_daemon_stop(&mut self, cx: &mut Context<Self>) {
        self.daemon_transition = Some(DaemonTransition::Stopping);
        self.daemon_transition_at = Some(std::time::Instant::now());
        // 用户级的 base：`stop` 停的是这个用户的 daemon，不是「这个数据目录的」。
        let user_dir = qcode_kernel::data_dir_hint(None);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { qcode_core::daemon::stop(&user_dir) })
                .await;
            this.update(cx, |shell, cx| {
                // 与 start 同一条规矩：`request_stop` 只是把停的信号送出去，进程还要
                // 一会儿才退出。过渡态等状态跟上（`!is_running`）才撤销，否则会露出
                // 「还绿着」的夹缝。
                shell.driver_watch_at = None;
                shell.refresh_driver_watch();
                match result {
                    Ok(true) => shell.ui.app.push_notice("定时任务驱动已停止".to_string()),
                    // `false` 不是错误：用户点下去时它可能刚好自己退了。
                    Ok(false) => {
                        shell.daemon_transition = None;
                        shell.ui.app.push_notice("驱动已经不在了".to_string())
                    }
                    Err(error) => {
                        shell.daemon_transition = None;
                        shell
                            .ui
                            .app
                            .push_notice(format!("无法停止定时任务驱动：{error}"))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // 显式导入，**不要**换成 `use super::*`：这个文件的父模块本身 `use super::*`
    // （也就是 `crate::*`），再套一层 glob 会让 rustc 在展开时崩掉（SIGBUS，
    // 2026-11-16 撞到过）。这不是风格偏好，是一个能让整条门禁红掉的编译器坑。
    use super::{DaemonButton, DaemonTransition, DriverTone, decide, status_sentence};
    use qcode_core::daemon::{Presence, Watch};
    use qcode_core::scheduler::DriverStatus;
    use time::macros::datetime;

    fn watch(daemon: Presence, driver: DriverStatus) -> Watch {
        Watch { driver, daemon }
    }

    fn at() -> time::PrimitiveDateTime {
        datetime!(2026-11-16 09:00:00)
    }

    /// **ADR-0098 §3 的那张表，四格逐格验。**
    ///
    /// 这是本 ADR 最值得测的一处：四格里有两格「不可点」，而它们的理由完全不同
    /// （窗口自己 / 别的目录）。画错一格的后果是用户点一个不动的按钮，或者去重建
    /// 一个还在跑的任务。
    #[test]
    fn the_four_cells_of_the_button() {
        let cases = [
            // (daemon, driver, 期望)
            (Presence::Absent, DriverStatus::Unknown, DaemonButton::Start),
            (
                Presence::Absent,
                DriverStatus::Gone { at: at() },
                DaemonButton::Start,
            ),
            (
                Presence::Absent,
                DriverStatus::Running { at: at() },
                DaemonButton::WindowDriven,
            ),
            (
                Presence::Absent,
                DriverStatus::Held { at: Some(at()) },
                DaemonButton::WindowDriven,
            ),
            (
                Presence::Running,
                DriverStatus::Running { at: at() },
                DaemonButton::Stop,
            ),
            (
                Presence::Running,
                DriverStatus::Held { at: Some(at()) },
                DaemonButton::Stop,
            ),
            (
                Presence::Running,
                DriverStatus::Unknown,
                DaemonButton::OccupiedElsewhere,
            ),
            (
                Presence::Running,
                DriverStatus::Gone { at: at() },
                DaemonButton::OccupiedElsewhere,
            ),
            (
                Presence::Unknown("nfs".into()),
                DriverStatus::Running { at: at() },
                DaemonButton::Unknown,
            ),
        ];
        for (daemon, driver, expected) in cases {
            let w = watch(daemon.clone(), driver);
            let got = decide(Some(&w), None);
            assert_eq!(
                got, expected,
                "daemon={daemon:?} driver={driver:?} should render {expected:?}"
            );
        }
    }

    /// **图标绿不绿，只看 `qcd` 在不在跑**（ADR-0100）。四格里只有两格是绿的。
    ///
    /// 这条是这次改动最容易画错的地方：段落那层灰问的是「任务会不会自己响」，
    /// 图标这层亮问的是「那个进程在不在」。两句话不是一回事，`WindowDriven` 就是
    /// 它们分叉的那一格——段落不灰（窗口在推时钟），图标灰（没有 qcd）。
    #[test]
    fn the_icon_is_lit_exactly_when_qcd_runs() {
        let lit = [
            DaemonButton::Stop,
            DaemonButton::OccupiedElsewhere,
            DaemonButton::Busy(DaemonTransition::Stopping),
        ];
        for button in lit {
            assert!(button.qcd_running(), "{button:?} should light the icon");
        }
        let dark = [
            DaemonButton::Start,
            DaemonButton::WindowDriven,
            DaemonButton::Unknown,
            DaemonButton::Busy(DaemonTransition::Starting),
        ];
        for button in dark {
            assert!(!button.qcd_running(), "{button:?} should grey the icon");
        }
        // 从 `Watch` 推出来的那一层也要一致：窗口自己在驱动时，图标必须是灰的。
        let window_driven = decide(
            Some(&Watch {
                driver: DriverStatus::Running { at: at() },
                daemon: Presence::Absent,
            }),
            None,
        );
        assert_eq!(window_driven, DaemonButton::WindowDriven);
        assert!(
            !window_driven.qcd_running(),
            "the window driving is not the daemon running"
        );
    }

    /// **除了过渡态，每一格都能点**（ADR-0100：乒乓键）。
    ///
    /// 旧版让四格里的三格不可点，包括 `WindowDriven`——那一格恰恰是最需要在
    /// 窗口里点的一格（点了就起一个排队的 daemon）。这条测试守住那次修正。
    #[test]
    fn only_a_transition_is_not_actionable() {
        for button in [
            DaemonButton::Start,
            DaemonButton::Stop,
            DaemonButton::WindowDriven,
            DaemonButton::OccupiedElsewhere,
            DaemonButton::Unknown,
        ] {
            assert!(button.actionable(), "{button:?} must be clickable");
        }
        for button in [
            DaemonButton::Busy(DaemonTransition::Starting),
            DaemonButton::Busy(DaemonTransition::Stopping),
        ] {
            assert!(!button.actionable(), "{button:?} must not be clickable");
        }
    }

    /// 提示语必须说出**点下去会发生什么**，而不只是描述状态。
    ///
    /// 一个只有图标的按钮没有文字兜底，提示就是它全部的说明书。
    #[test]
    fn every_tooltip_says_what_a_click_does() {
        for button in [
            DaemonButton::Start,
            DaemonButton::Stop,
            DaemonButton::WindowDriven,
            DaemonButton::OccupiedElsewhere,
            DaemonButton::Unknown,
        ] {
            let tip = button.tooltip();
            assert!(
                tip.contains("点一下"),
                "{button:?} must say what a click does, got {tip:?}"
            );
        }
        // 亮度也要在里面说清，否则「灰」永远是个谜。
        assert!(DaemonButton::Start.tooltip().contains("灰"));
        assert!(DaemonButton::Stop.tooltip().contains("绿"));
        assert!(DaemonButton::WindowDriven.tooltip().contains("灰"));
    }

    /// 状态词不能重复、不能为空：它同时是 `/daemon status` 的第一句和这颗图标
    /// 唯一的无障碍名字。
    #[test]
    fn every_cell_has_its_own_state_word() {
        let words = [
            DaemonButton::Start,
            DaemonButton::Stop,
            DaemonButton::WindowDriven,
            DaemonButton::OccupiedElsewhere,
            DaemonButton::Unknown,
            DaemonButton::Busy(DaemonTransition::Starting),
            DaemonButton::Busy(DaemonTransition::Stopping),
        ]
        .map(|button| button.state_word());
        for (index, word) in words.iter().enumerate() {
            assert!(!word.is_empty());
            assert!(
                !words[index + 1..].contains(word),
                "{word:?} is used twice; a screen reader would give two states one name"
            );
        }
    }

    /// 过渡态压过一切：`stop` 在途中不该显示成「停止」（否则用户会再点一次）。
    #[test]
    fn a_transition_hides_the_steady_state() {
        let w = watch(Presence::Running, DriverStatus::Running { at: at() });
        assert_eq!(
            decide(Some(&w), Some(DaemonTransition::Stopping)),
            DaemonButton::Busy(DaemonTransition::Stopping)
        );
        assert_eq!(
            decide(None, Some(DaemonTransition::Starting)),
            DaemonButton::Busy(DaemonTransition::Starting)
        );
    }

    /// `Held` 是警告，`Gone` / `Unknown` 才是灰。合盖的笔记本不该被画成停摆。
    #[test]
    fn a_drowsy_driver_is_not_grey() {
        let tone = |driver| {
            crate::shell::daemon::tone(Some(&Watch {
                driver,
                daemon: Presence::Running,
            }))
        };
        assert_eq!(tone(DriverStatus::Running { at: at() }), DriverTone::Live);
        assert_eq!(
            tone(DriverStatus::Held { at: None }),
            DriverTone::Drowsy,
            "a suspended driver must not look stopped"
        );
        assert_eq!(tone(DriverStatus::Gone { at: at() }), DriverTone::Dormant);
        assert_eq!(tone(DriverStatus::Unknown), DriverTone::Dormant);
        // 没有后端 = 不知道 = 保守地按「不会触发」画，但配上说明行。
        assert_eq!(crate::shell::daemon::tone(None), DriverTone::Dormant);
    }

    /// 说明行不能编：`Running` 时必须说「会触发」，`Gone` 时必须说「不会」，
    /// 而且必须点出「不补跑」。
    #[test]
    fn the_status_sentence_matches_the_state() {
        let running = status_sentence(Some(&watch(
            Presence::Running,
            DriverStatus::Running { at: at() },
        )));
        assert!(running.contains("会正常投递"), "{running}");

        let gone = status_sentence(Some(&watch(
            Presence::Absent,
            DriverStatus::Gone { at: at() },
        )));
        assert!(gone.contains("不在跑") || gone.contains("不会"), "{gone}");
        assert!(gone.contains("补跑") || gone.contains("错过"), "{gone}");

        // `Held` 单独一档：说清「还在，只是睡着了」，且不许说「错过后补」那套。
        let held = status_sentence(Some(&watch(
            Presence::Running,
            DriverStatus::Held { at: None },
        )));
        assert!(held.contains("心跳已过期"), "{held}");
        assert!(held.contains("唤醒后会继续"), "{held}");

        // daemon 在别的目录：说明行必须解释「为什么这里不响」。
        let elsewhere = status_sentence(Some(&watch(Presence::Running, DriverStatus::Unknown)));
        assert!(elsewhere.contains("别的数据目录"), "{elsewhere}");
    }
}
