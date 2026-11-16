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
    /// 能点吗。**只有 Start 与 Stop 能点**——这正是「灰是标记，不是许可撤销」的
    /// 另一半：不能点的只有这一颗按钮，列表不受影响（ADR-0098 §4）。
    pub(crate) fn actionable(&self) -> bool {
        matches!(self, Self::Start | Self::Stop)
    }

    /// 悬停提示。说清「为什么不能点」，而不是让用户自己猜。
    pub(crate) fn tooltip(&self) -> &'static str {
        match self {
            Self::Start => "启动定时任务驱动（关掉窗口后任务仍会触发）",
            Self::Stop => "停止定时任务驱动（到点不再触发；任务与列表都还在）",
            Self::WindowDriven => "现在由这个窗口驱动；关掉窗口任务就不跑了",
            Self::OccupiedElsewhere => "驱动在跑，但服务的是另一个数据目录",
            Self::Unknown => "查不出驱动状态",
            Self::Busy(DaemonTransition::Starting) => "正在启动…",
            Self::Busy(DaemonTransition::Stopping) => "正在停止…",
        }
    }

    /// 图标下的短标签。侧栏很窄，所以三个字以内。
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Start => "启动",
            Self::Stop => "停止",
            Self::WindowDriven => "窗口",
            Self::OccupiedElsewhere | Self::Unknown => "—",
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
    }

    /// 点段头那个按钮。
    pub(crate) fn toggle_daemon(&mut self, cx: &mut Context<Self>) {
        match self.daemon_button() {
            DaemonButton::Start => self.request_daemon_start(cx),
            DaemonButton::Stop => self.request_daemon_stop(cx),
            // 不能点的几格：给一句为什么，而不是静默。
            other => {
                let why = match other {
                    DaemonButton::WindowDriven => {
                        "现在由这个窗口驱动。关闭窗口后任务就不再触发——那时可以启动常驻驱动。"
                    }
                    DaemonButton::OccupiedElsewhere => {
                        "驱动在跑，但服务的是另一个数据目录（一个用户只有一个）。\
                         要它服务这里，先用 `qc daemon stop` 停掉。"
                    }
                    DaemonButton::Unknown => "查不出驱动状态，先确认 `qc daemon status` 说了什么。",
                    DaemonButton::Busy(_) => "正在切换，等它完成。",
                    DaemonButton::Start | DaemonButton::Stop => unreachable!("handled above"),
                };
                self.ui.app.push_notice(why.to_string());
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
                    .push_notice(format!("{} · {sentence}", button.label()));
                cx.notify();
            }
            qcode_ui_state::DaemonVerb::Start => self.request_daemon_start(cx),
            qcode_ui_state::DaemonVerb::Stop => self.request_daemon_stop(cx),
        }
    }

    pub(crate) fn request_daemon_start(&mut self, cx: &mut Context<Self>) {
        self.daemon_transition = Some(DaemonTransition::Starting);
        let data_dir = self.data_dir.clone();
        // 起进程会阻塞（找二进制、fork、等锁）：放到后台执行器，界面这一帧照画。
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { qcode_core::daemon::start(&data_dir, None) })
                .await;
            this.update(cx, |shell, cx| {
                shell.daemon_transition = None;
                // 无论成败都立刻重探：成功时立刻显示「停止」，失败时回到「启动」并
                // 说清为什么——那比一个停在「启动中」的按钮诚实。
                shell.driver_watch_at = None;
                shell.refresh_driver_watch();
                match result {
                    Ok(pid) => shell
                        .ui
                        .app
                        .push_notice(format!("定时任务驱动已启动（pid {pid}）")),
                    Err(error) => shell
                        .ui
                        .app
                        .push_notice(format!("无法启动定时任务驱动：{error}")),
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
        // 用户级的 base：`stop` 停的是这个用户的 daemon，不是「这个数据目录的」。
        let user_dir = qcode_kernel::data_dir_hint(None);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { qcode_core::daemon::stop(&user_dir) })
                .await;
            this.update(cx, |shell, cx| {
                shell.daemon_transition = None;
                shell.driver_watch_at = None;
                shell.refresh_driver_watch();
                match result {
                    Ok(true) => shell.ui.app.push_notice("定时任务驱动已停止".to_string()),
                    // `false` 不是错误：用户点下去时它可能刚好自己退了。
                    Ok(false) => shell.ui.app.push_notice("驱动已经不在了".to_string()),
                    Err(error) => shell
                        .ui
                        .app
                        .push_notice(format!("无法停止定时任务驱动：{error}")),
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

    /// 只有两颗按钮能点。这条钉住「灰是标记，不是许可撤销」的另一半：
    /// 不能点的**只有**这一颗，列表不受影响（ADR-0098 §4）。
    #[test]
    fn only_start_and_stop_are_actionable() {
        assert!(DaemonButton::Start.actionable());
        assert!(DaemonButton::Stop.actionable());
        for button in [
            DaemonButton::WindowDriven,
            DaemonButton::OccupiedElsewhere,
            DaemonButton::Unknown,
            DaemonButton::Busy(DaemonTransition::Starting),
            DaemonButton::Busy(DaemonTransition::Stopping),
        ] {
            assert!(!button.actionable(), "{button:?} must not be clickable");
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
