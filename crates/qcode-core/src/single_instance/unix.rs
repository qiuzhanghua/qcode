//! Unix 端：`flock` + 文件路径 UDS。
//!
//! 不用 abstract namespace：macOS 没有它，而这条代码要在三端都成立。

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Request, request_line};

/// 通道在 data dir 里的扩展名。完整名字是 `<scope>.sock`——GUI 作用域下正是
/// ADR-0083 落地时的 `gui.sock`，改名会让升级期间的两个二进制互相看不见。
pub(super) const CHANNEL_EXT: &str = "sock";

/// 单次连接 / 读写的上限。GUI 就在本机，跨进程握手不该慢；给一个上限是为了
/// 让「对面卡住」变成一次明确的失败，而不是无限等。
const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// Unix 上通道就是文件路径本身。
pub(super) fn channel_address(paths: &super::Paths) -> String {
    paths.channel.display().to_string()
}

/// `sockaddr_un.sun_path` 的容量，**含结尾 NUL**（ADR-0099）。
///
/// 超了就是 `bind` 报 `AF_UNIX path too long`——而这个错误发生在 `listen` 里，于
/// 是 `acquire_in` 退回 `Disabled`：daemon 照常跑，只是完全没有互斥。那正是
/// ADR-0098「已知缺陷」记录的那次。
#[cfg(target_os = "linux")]
const SUN_PATH: usize = 108;
#[cfg(not(target_os = "linux"))]
const SUN_PATH: usize = 104;

/// 这条路径装得进 `sun_path` 吗（按字节，因为 `sun_path` 是字节数组）。
///
/// 上限**含**结尾 NUL，所以字符串本身要比 `SUN_PATH` 少一个字节：`< SUN_PATH`
/// 就是「加上 NUL 仍装得下」。
fn fits(path: &Path) -> bool {
    path.as_os_str().len() < SUN_PATH
}

/// 通道落在哪儿——装得进 data dir 就用它，装不进才换地方（ADR-0099 §2）。
pub(super) fn channel_path(scope: super::Scope, data_dir: &Path) -> (PathBuf, Option<String>) {
    let inside = super::channel_in_data_dir(scope, data_dir);
    if fits(&inside) {
        return (inside, None);
    }
    match private_socket(scope, data_dir) {
        Ok(path) => {
            let why = format!(
                "{} is {} bytes but the platform limit is {} — channel moved to {}",
                inside.display(),
                inside.as_os_str().len(),
                SUN_PATH - 1,
                path.display()
            );
            (path, Some(why))
        }
        // 造不出短目录就退回原路径：`listen` 会失败，于是调用方得到一句带真实原因的
        // `Disabled`。这里**不**假装成功——假装成功的后果是「有个没人能连的通道」。
        Err(error) => {
            let why = format!(
                "cannot prepare a short socket directory ({error}); {} does not fit in {} bytes",
                inside.display(),
                SUN_PATH - 1
            );
            (inside, Some(why))
        }
    }
}

/// 回退目录必须**每用户私有**：它收 `Stop` 请求。
///
/// 放在共享 `TMPDIR` 的顶层，别的本地用户可以先 `bind` 这条路然后选择不理你——
/// 那是 DoS，而且他能看见你的请求。所以目录名带 uid、权限 `0700`、复用时校验。
///
/// # 为什么还要再按身份分一层
///
/// 只用一个 `/tmp/qcode-<uid>/` 的话，**同一个用户的全部 data dir 共用一个目录**：
/// 任何东西把它占住（一个同名文件、一个被删了又重建的目录），所有「路径太长」的实例
/// 一起失去互斥。按身份再分一层，坏掉的只是那一个。
///
/// 这一点是测试先撞出来的：几条并发跑的用例共用一个目录，其中一条把目录删掉换成
/// 文件，另一条立刻 `bind` 失败。**共享的可变全局状态在测试里会互相破坏，在生产里
/// 也一样**——生产里的「另一个东西」是别的 data dir。
fn private_socket(scope: super::Scope, data_dir: &Path) -> io::Result<PathBuf> {
    let fingerprint = super::stable_hash(&data_dir.to_string_lossy());
    let mut last: Option<io::Error> = None;
    for base in short_bases() {
        // 两层都要私有：外层是「我的」，内层是「我这个 data dir 的」。
        let ours = base.join(format!("qcode-{}", uid()));
        let slot = ours.join(format!("{fingerprint:016x}"));
        let path = slot.join(format!("{}.sock", scope.prefix()));
        // 回退目录本身也可能太长（`$TMPDIR` 在某些管家里很深）。装不下就试下一个。
        if !fits(&path) {
            continue;
        }
        match ensure_private(&ours).and_then(|()| ensure_private(&slot)) {
            Ok(()) => return Ok(path),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            qcode_i18n::trp(
                "core.err.single_instance.socket_path_too_long",
                &[SUN_PATH.to_string()],
            ),
        )
    }))
}

/// 候选根目录，**顺序即优先级**。
///
/// `/tmp` 排第一不是为了短，而是为了**确定性**：`$TMPDIR` 是环境变量，从 Finder
/// 启动的 GUI 与从终端启动的 `qc` 可能拿到不同的值。地址一旦依赖它，「同一个
/// 实例」就会有两个地址，而症状是 `daemon stop` 安静地够不着对面。
fn short_bases() -> Vec<PathBuf> {
    let mut bases = vec![PathBuf::from("/tmp")];
    let tmp = std::env::temp_dir();
    if tmp.as_path() != Path::new("/tmp") {
        bases.push(tmp);
    }
    bases
}

fn uid() -> u32 {
    // SAFETY: `getuid` 无参数、无副作用，永远成功。
    unsafe { libc::getuid() }
}

/// 建出（或复用）一个只有自己能进的目录。
///
/// 复用时的三条校验：是目录、属主是自己、权限位不含 group/other。前两条不符就
/// **拒绝**——那不是我们建的东西，用它等于把自己的控制通道交出去。第三条可以
/// 自己修（收紧是我们能做的、且不改变属主语义）。
fn ensure_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    // `symlink_metadata`：一个指向别处的软链不该被当成「我们的目录」。
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            qcode_i18n::trp(
                "core.err.single_instance.not_a_directory",
                &[dir.display().to_string()],
            ),
        ));
    }
    if meta.uid() != uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            qcode_i18n::trp(
                "core.err.single_instance.foreign_owner",
                &[
                    dir.display().to_string(),
                    meta.uid().to_string(),
                    uid().to_string(),
                ],
            ),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 持有的锁。**drop 就是放锁**——这正是我们要的语义：进程怎么死的都行
/// （`exit()`、`kill -9`、崩溃），内核在 fd 关闭时自动释放。
pub(super) struct LockGuard {
    _file: std::fs::File,
}

pub(super) fn lock(path: &Path) -> io::Result<Option<LockGuard>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    // SAFETY: `file` 在本次调用期间活着，fd 有效。
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(LockGuard { _file: file }));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // macOS 报 EWOULDBLOCK，Linux 两个都可能出现。
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(None),
        _ => Err(error),
    }
}

/// 连上去发一条请求。
///
/// 三种结果都要分清：
///
/// - `Ok(true)`：对面确认收到。**这是「有活实例」的确证**——对 `Activate` 与
///   `Stop` 都是。`Stop` 能被确认，是这个协议相对信号的全部好处。
/// - `Ok(false)`：连不上（`ECONNREFUSED` 说明文件是残留的、没有监听者）。
/// - `Err`：链路层面的意外（超时、写到一半断）。调用方退回按锁判定。
pub(super) fn try_request(paths: &super::Paths, request: Request) -> io::Result<bool> {
    let mut stream = match UnixStream::connect(&paths.channel) {
        Ok(stream) => stream,
        Err(error) if is_absent(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(request_line(request).as_bytes())?;
    stream.flush()?;
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line)?;
    let ok = serde_json::from_str::<serde_json::Value>(line.trim())
        .ok()
        .and_then(|value| value.get("ok").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    if ok {
        Ok(true)
    } else {
        // 连上了却不是我们的协议：把它当「没人能接活」，别硬说成功。
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            qcode_i18n::trp(
                "core.err.single_instance.unexpected_reply",
                &[paths.channel.display().to_string(), format!("{line:?}")],
            ),
        ))
    }
}

/// 「这个地址上没有活实例」——文件不存在，或者存在但没人监听。
///
/// 第二种正是要**当没有**处理的：残留文件不该让下一个人起不来。
fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// 监听端。持有它就等于「这条地址归我」。
pub(super) struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

pub(super) fn listen(paths: &super::Paths) -> io::Result<Listener> {
    if let Some(dir) = paths.channel.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 能走到这里说明锁在我手上，所以这条残留文件一定没有活监听者
    // （只有持锁之后才 listen）。直接删掉重建。
    match std::fs::remove_file(&paths.channel) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(&paths.channel)?;
    Ok(Listener {
        listener,
        path: paths.channel.clone(),
    })
}

impl Listener {
    pub(super) fn accept_loop<F: Fn(super::Request, &mut dyn io::Write) -> bool>(
        self,
        on_request: F,
    ) {
        for stream in self.listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // 一条连接一个请求，处理完就断：协议只有一件事，不需要长连接，
            // 而长连接会引入「谁先关」的状态机。
            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
            let mut line = String::new();
            let mut reader = match stream.try_clone() {
                Ok(copy) => BufReader::new(copy),
                Err(_) => continue,
            };
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let Some(request) = super::Request::parse(&line) else {
                continue;
            };
            // 应答由回调写（见 `Primary::serve`）：`Stop` 必须先确认再退出，否则
            // 客户端分不清「没听到」与「听到了但来不及说」。
            let keep_going = on_request(request, &mut stream);
            // 接收端不再接活（窗口关了、daemon 要退）就安静退出，别攒一堆没人
            // 认领的通知。
            if !keep_going {
                return;
            }
        }
        // 走到这里说明 accept 出错了（例如 fd 被关）。把 socket 文件收掉，
        // 免得下一次启动先撞一次 ECONNREFUSED —— 那虽然会被正确当成「没
        // 人在听」，但多一层无谓的路径。
        let _ = std::fs::remove_file(&self.path);
        // 回退目录（ADR-0099）也收掉：`remove_dir` 只在**空**的时候成功，所以
        // 它天然地「只在没有别人共用时」生效。不清理的话，每个用过的长 data dir
        // 会在 /tmp 里留下一个空目录，越攒越多。
        //
        // `kill -9` 时这一段不会跑，残留就留着——那与锁文件同一个道理：标记，不是
        // 状态，不需要为一个「下次会覆盖」的东西写清理器。
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}
