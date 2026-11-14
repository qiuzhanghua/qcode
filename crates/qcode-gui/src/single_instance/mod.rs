//! GUI 单实例（ADR-0083）与跨进程激活（ADR-0084）。
//!
//! 两件事放在一起，是因为它们共用一组位置（`<data_dir>/locks/`）和一次启动
//! 顺序；拆开会让「先连后退」这个顺序散在两个模块里。
//!
//! # 为什么必须自建
//!
//! macOS 上「双击图标不该开第二个」看起来该由 LaunchServices 免费提供，实测
//! （ADR-0084 实测 D）却是：**两个由终端直接 `exec` 的实例都被登记成
//! `com.qcode.desktop`（count = 2）**。直接 exec 完全绕开 LaunchServices 的
//! 实例管理，而 qcode 的 `.app` 启动器与开发者从终端起 `qc gui` 都走直接 exec。
//!
//! # 为什么锁是 OS 级文件锁，而不是 pid 文件
//!
//! ADR-0082 那次发现：macOS 的 `-[NSApplication terminate:]` 由 AppKit 调
//! `exit()`，析构不跑、`atexit` 之外的清理不跑。任何「退出时主动解锁 / 删锁
//! 文件」的逻辑在这一端都不可靠；而 OS 级锁在进程消失（含 `kill -9`、崩溃、
//! 断电重启）时**由内核自动释放**，是三端唯一语义一致的答案。代价是锁文件会
//! 一直留在磁盘上——那是标记，不是状态，不需要清理。
//!
//! # 为什么激活必须走自建通道
//!
//! 实测 E（ADR-0084）：关窗走的是 `orderOut:`（真隐藏，ADR-0052），此后
//! `isActive` 与 `NSWorkspace.frontmost` 都报 true 而屏幕上没有窗口，于是
//! `open` 与 `NSRunningApplication::activate` **双双「成功返回、什么也不发生」**。
//! 只有进程内的 `makeKeyAndOrderFront:` 能把窗口拿回来，所以激活只能由第一个
//! 进程自己执行，因此必须有跨进程通道。

use std::io;
use std::path::{Path, PathBuf};

pub use crate::tray::TrayCommand;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as backend;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as backend;

#[cfg(not(any(unix, windows)))]
mod fallback;
#[cfg(not(any(unix, windows)))]
use fallback as backend;

/// 拿锁的结果。
///
/// `Debug` 是给测试的失败信息用的：`expect_err` 要求可以把「本不该出现的
/// 成功分支」打印出来。这里只打印形状，不碰里面的 fd / 监听者。
pub enum Outcome {
    /// 抢到了：本进程就是那唯一一个 GUI。**必须持有到进程结束**——
    /// 掉了 `Primary` 就等于放锁。
    Primary(Primary),
    /// 另一个实例在跑，并且它确认已经把窗口拿到前台了。调用方安静地
    /// `exit 0`（脚本里 `qc gui` 不该报错）。
    HandedOver,
    /// 互斥机制本身不可用（NFS、权限、目录不可写）。调用方照常启动，
    /// 但要说明白为什么这一次没有保护。
    Disabled(String),
}

impl std::fmt::Debug for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Primary(_) => f.write_str("Primary"),
            Self::HandedOver => f.write_str("HandedOver"),
            Self::Disabled(why) => write!(f, "Disabled({why})"),
        }
    }
}

impl Outcome {
    /// 给日志用的一句话；`Primary` 不需要解释。
    pub fn describe(&self) -> Option<String> {
        match self {
            Self::Primary(_) => None,
            Self::HandedOver => Some("another instance owns the GUI and raised its window".into()),
            Self::Disabled(why) => Some(format!("single-instance check skipped: {why}")),
        }
    }
}

/// 抢到的锁与那条通道。
///
/// 字段故意不暴露：持有它就等于持有锁，这一点不能靠调用方自觉。
pub struct Primary {
    /// 锁句柄。**只靠它活着**——OS 锁随 fd / handle 关闭而释放，所以这个字段
    /// 不能被 drop 掉。`serve` 会把它移进监听线程，让它的生命周期等于进程。
    _lock: backend::LockGuard,
    listener: backend::Listener,
}

impl Primary {
    /// 开始接活：收到 `{"activate":true}` 就往 `tx` 里塞一条
    /// [`TrayCommand::ShowWindow`]。
    ///
    /// 塞进**托盘那条通道**是有意的（ADR-0084 §3）：托盘「显示主窗口」已经
    /// 处理了三端各自「原生调用必须推迟到 executor」的坑，另写一条「把窗口弄
    /// 出来」的路只会有第二份平台细节和第二个 bug。
    ///
    /// 拿不到 `tx` 的场合（托盘注册失败、`--help` 之类根本不开窗）不会走到这里。
    pub fn serve(self, tx: std::sync::mpsc::Sender<TrayCommand>) {
        let Self { _lock, listener } = self;
        // 线程故意 detach：它要活到进程结束，而进程结束时 OS 会替我们收摊
        // （连接断开、锁释放）。`_lock` 随线程一起被持有。
        std::thread::Builder::new()
            .name("qcode-gui-activate".into())
            .spawn(move || {
                let _lock = _lock;
                listener.accept_loop(tx);
            })
            .ok();
    }
}

/// 这个 data dir 对应的锁与通道位置。
///
/// 作用域是 `canonical(data_dir)` × 显示会话（ADR-0083 §2）：data dir 决定
/// 「哪一份状态」，两个不同 data dir 的 GUI 写两份互不相干的 store，合成一个
/// 语义是错的；而默认 data dir 就是 `~/.local/share/qcode`，所以默认行为恰好
/// 等于用户要的「每机每用户一个」。显示会话是给激活用的：第一个实例在另一个
/// X server / 另一个 Windows 登录会话上时，「把窗口拿到前台」物理上做不到。
#[derive(Clone, Debug)]
pub struct Paths {
    /// canonical 之后的 data dir。整个作用域都是它算出来的（ADR-0083 §2），
    /// 所以留着它，而不是让各端从 `channel` 的父目录往回猜。
    pub data_dir: PathBuf,
    pub lock: PathBuf,
    pub channel: PathBuf,
}

impl Paths {
    /// `data_dir` 应当**已经 canonical**（调用方用 `resolve_data_dir` 拿到它）。
    /// 这里再 canonicalize 一次，因为软链会让同一个目录出现两个不同的键。
    pub fn for_data_dir(data_dir: &Path) -> Self {
        let base = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
        let locks = base.join("locks");
        Self {
            lock: locks.join("gui.lock"),
            channel: locks.join(backend::CHANNEL_NAME),
            data_dir: base,
        }
    }

    /// 通向另一个实例的那条通道地址。Unix 上就是 `channel` 路径本身；
    /// Windows 上是命名管道名（带 data dir 的哈希），两者形状不同，
    /// 所以统一叫「地址」而不是「路径」。
    pub fn channel_address(&self) -> String {
        backend::channel_address(self)
    }
}

/// 退避重试的次数与间隔（ADR-0084 §2 第 3 步）。
///
/// 覆盖的是这个窗口：第一个进程**刚拿到锁、还没来得及 `listen`**。10 × 100ms
/// 足够盖住它，又不至于让用户觉得双击没反应。
const RETRIES: usize = 10;
const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// 逃生口。一个坏掉的锁能把用户彻底锁在 GUI 外面，而 GUI 正是他用来排查问题
/// 的地方——所以必须有个开关（ADR-0083 §4）。
///
/// 这不是留后门：写死 `1` 等于「永远跳过」，写 `0` 是显式要的例外。
fn disabled_by_env() -> bool {
    matches!(
        std::env::var("QC_GUI_SINGLE_INSTANCE").as_deref(),
        Ok("0") | Ok("false") | Ok("no")
    )
}

/// 按 ADR-0084 §2 的顺序拿锁 / 让位。
///
/// ```text
/// 1. 连通道 ──连上──→ 发 activate，等回复 → HandedOver
///       │连不上
///       ↓
/// 2. 抢锁 ──抢到──→ 清掉残留通道文件 → listen → Primary
///       │抢不到
///       ↓
/// 3. 回到 1，退避重试 → 仍失败则报错（**绝不接管**）
/// ```
///
/// 先连后退的理由：**「通道能连上」是「有活实例」的确证**，信息量比「锁被占」
/// 强——锁被占可能只是第一个进程刚拿锁、还没来得及 `listen`。
///
/// 第 3 步失败时**不许接管启动第二个**：那会把「每机每用户一个」破掉，而用户
/// 下次只会看到两个窗口，根本想不到是启动时的一次超时。
pub fn acquire(data_dir: &Path) -> io::Result<Outcome> {
    if disabled_by_env() {
        return Ok(Outcome::Disabled("QC_GUI_SINGLE_INSTANCE=0".into()));
    }
    let paths = Paths::for_data_dir(data_dir);
    if let Some(dir) = paths.lock.parent() {
        // 建不出来不是错误：锁会自己报出来，而「目录不可写」正是要退回
        // Disabled 的那一类。
        let _ = std::fs::create_dir_all(dir);
    }

    // 同一进程内第二次 acquire：POSIX 的 flock 对同一进程的第二个 fd 会
    // **成功**（锁属于进程，不是 fd），Windows 的 LockFileEx 同理。ADR-0083
    // 把这一条列为必须钉住的坑——两个 GUI 线程各自以为抢到了锁，正是因为
    // OS 不拦。这里用进程内的登记表补上。
    if !claim_in_process(&paths.lock) {
        return Ok(Outcome::HandedOver);
    }

    let mut attempt = 0;
    loop {
        // 1. 先连：连上就是有活实例。
        match backend::try_activate(&paths) {
            Ok(true) => {
                release_in_process(&paths.lock);
                return Ok(Outcome::HandedOver);
            }
            Ok(false) => {}
            // 通道坏了不等于锁坏了；继续按锁判定。
            Err(error) => tracing::debug!(%error, "gui: activate channel unusable"),
        }

        // 2. 再抢锁。
        match backend::lock(&paths.lock) {
            Ok(Some(guard)) => {
                // 抢到锁 ⇒ 一定没有活实例 ⇒ 残留的通道文件是我的了
                // （只有持锁之后才 listen，所以能 listen 就一定没人听）。
                let listener = match backend::listen(&paths) {
                    Ok(listener) => listener,
                    Err(error) => {
                        release_in_process(&paths.lock);
                        return Ok(Outcome::Disabled(format!(
                            "cannot listen on {}: {error}",
                            paths.channel_address()
                        )));
                    }
                };
                return Ok(Outcome::Primary(Primary {
                    _lock: guard,
                    listener,
                }));
            }
            Ok(None) => {}
            Err(error) => {
                // 锁机制本身坏了（NFS、权限）：别把用户锁在门外。
                release_in_process(&paths.lock);
                return Ok(Outcome::Disabled(format!(
                    "cannot lock {}: {error}",
                    paths.lock.display()
                )));
            }
        }

        // 3. 回到 1。
        if attempt >= RETRIES {
            release_in_process(&paths.lock);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "another qcode GUI holds {} but did not answer on {} within {}ms; \
                     not starting a second instance (set QC_GUI_SINGLE_INSTANCE=0 to override)",
                    paths.lock.display(),
                    paths.channel_address(),
                    RETRIES * RETRY_INTERVAL.as_millis() as usize
                ),
            ));
        }
        attempt += 1;
        std::thread::sleep(RETRY_INTERVAL);
    }
}

/// 进程内「谁已经拿到了哪个锁」的登记表。
///
/// 存在的唯一理由是 OS 不拦同进程的第二次加锁（见 [`acquire`] 里的注释）。
/// 按锁路径而不是全局布尔值记：同一个进程里两个**不同** data dir 的 GUI 应当
/// 都能起来，那正是 `--data-dir` 的用处（调试、CI）。
fn in_process() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// 记下「本进程持有这个锁」。已经有人记过就返回 `false`。
fn claim_in_process(lock: &Path) -> bool {
    match in_process().lock() {
        Ok(mut held) => held.insert(lock.to_path_buf()),
        // 毒化的锁不该把 GUI 挡在门外：这一层是**加固**，不是唯一防线。
        Err(_) => true,
    }
}

fn release_in_process(lock: &Path) {
    if let Ok(mut held) = in_process().lock() {
        held.remove(lock);
    }
}

/// 协议：一行 JSON，只做一件事（ADR-0084 §3）。
///
/// 不传工作目录、不传参数、不传会话 id：第二实例的 cwd 与第一实例不同时只
/// **浮窗**，不切 workspace（切 workspace 会打断正在跑的 turn）。
pub(crate) const ACTIVATE_REQUEST: &str = "{\"activate\":true}\n";
pub(crate) const ACTIVATE_REPLY: &str = "{\"ok\":true}\n";

/// `activate` 这一行是不是我们要的请求。宽进（多空白、少换行都认），
/// 因为对面可能是别的版本、或者是人手 `nc` 进去试的。
pub(crate) fn is_activate_request(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return false;
    };
    value.get("activate").and_then(|v| v.as_bool()) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activate_request_is_parsed_leniently() {
        assert!(is_activate_request(ACTIVATE_REQUEST));
        assert!(is_activate_request("{\"activate\":true}"));
        assert!(is_activate_request("  {\"activate\" : true}  "));
        // 其余一律不认：宁可不浮窗，也不能把一条乱码当命令执行。
        assert!(!is_activate_request("{\"activate\":false}"));
        assert!(!is_activate_request("{\"activate\":\"true\"}"));
        assert!(!is_activate_request("{\"quit\":true}"));
        assert!(!is_activate_request("activate"));
        assert!(!is_activate_request(""));
    }

    #[test]
    fn paths_are_under_the_data_dir() {
        let dir = std::path::PathBuf::from("/tmp/qc-si-paths");
        let paths = Paths::for_data_dir(&dir);
        assert_eq!(paths.lock, dir.join("locks").join("gui.lock"));
    }

    #[test]
    fn second_claim_in_the_same_process_fails() {
        // ADR-0083 代价一节点名要钉住的那条：OS 允许同进程二次加锁，
        // 所以这一层必须自己拦。
        let lock = std::path::PathBuf::from("/tmp/qc-si-in-process.lock");
        assert!(claim_in_process(&lock));
        assert!(!claim_in_process(&lock), "同进程第二次必须失败");
        release_in_process(&lock);
        assert!(claim_in_process(&lock), "放掉之后应当能再拿");
        release_in_process(&lock);
    }

    #[test]
    fn different_data_dirs_do_not_collide() {
        let a = std::path::PathBuf::from("/tmp/qc-si-a.lock");
        let b = std::path::PathBuf::from("/tmp/qc-si-b.lock");
        assert!(claim_in_process(&a));
        assert!(claim_in_process(&b), "不同 data dir 必须都能起来");
        release_in_process(&a);
        release_in_process(&b);
    }
}
