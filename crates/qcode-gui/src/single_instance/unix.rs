//! Unix 端：`flock` + 文件路径 UDS。
//!
//! 不用 abstract namespace：macOS 没有它，而这条代码要在三端都成立。

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{ACTIVATE_REPLY, ACTIVATE_REQUEST, TrayCommand, is_activate_request};

/// 通道在 data dir 里的文件名。
pub(super) const CHANNEL_NAME: &str = "gui.sock";

/// 单次连接 / 读写的上限。GUI 就在本机，跨进程握手不该慢；给一个上限是为了
/// 让「对面卡住」变成一次明确的失败，而不是无限等。
const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// Unix 上通道就是文件路径本身。
pub(super) fn channel_address(paths: &super::Paths) -> String {
    paths.channel.display().to_string()
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

/// 连上去发一次 `activate`。
///
/// 三种结果都要分清：
///
/// - `Ok(true)`：对面确认收到。**这是「有活实例」的确证。**
/// - `Ok(false)`：连不上（`ECONNREFUSED` 说明文件是残留的、没有监听者）。
/// - `Err`：链路层面的意外（超时、写到一半断）。调用方退回按锁判定。
pub(super) fn try_activate(paths: &super::Paths) -> io::Result<bool> {
    let mut stream = match UnixStream::connect(&paths.channel) {
        Ok(stream) => stream,
        Err(error) if is_absent(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(ACTIVATE_REQUEST.as_bytes())?;
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
            format!(
                "unexpected reply from {}: {line:?}",
                paths.channel.display()
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
    pub(super) fn accept_loop(self, tx: std::sync::mpsc::Sender<TrayCommand>) {
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
            if !is_activate_request(&line) {
                continue;
            }
            // 通道到人没了不能挂：接收端（窗口）已经不在了，安静退出。
            if tx.send(TrayCommand::ShowWindow).is_err() {
                return;
            }
            let _ = stream.write_all(ACTIVATE_REPLY.as_bytes());
            let _ = stream.flush();
        }
        // 走到这里说明 accept 出错了（例如 fd 被关）。把 socket 文件收掉，
        // 免得下一次启动先撞一次 ECONNREFUSED —— 那虽然会被正确当成「没
        // 人在听」，但多一层无谓的路径。
        let _ = std::fs::remove_file(&self.path);
    }
}
