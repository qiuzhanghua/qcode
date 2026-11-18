//! 定时任务表的文件监听：**谁写 `tasks.json`，UI 就刷新**（ADR-0092 §6 的补全）。
//!
//! # 为什么需要它
//!
//! 驱动互斥（ADR-0091）之后，持有调度时钟的进程有两种：GUI 自己，或者脱离 GUI
//! 常驻的 `qcd`。GUI 自己开车时，`Host::changed` 会即时发 `ScheduleChanged`；
//! 但 `qcd` 开车时它**不通知任何人**（`qcd::host::changed` 阶段 2 只写日志），
//! 于是 qcd 写回执 / 推进任务之后，GUI 的详情页与运行记录停留在旧快照。
//!
//! 这个模块补上的正是那一半：监听 `schedule/tasks.json` 所在的目录，任何进程
//! 写这张表（含原子替换）时由**操作系统**通知 GUI。通知来自 OS 而不是 qcd，
//! 所以「qcd 脱离 qcode 运行」「qcode 重启后再挂监听」都天然成立——双方唯一的
//! 交集是磁盘上的那张表，不需要任何进程间连接。
//!
//! # 为什么监听目录而不是文件
//!
//! [`SessionStore::save_schedule_tasks`] 写表是「临时文件 + rename」原子写，
//! rename 会替换 inode——监听文件本身会漏掉那次替换。所以这里监听父目录，
//! 再按事件里的文件名 == `tasks.json` 过滤。`.tmp` 的中间态自然被滤掉。
//!
//! # 平台
//!
//! 主平台全部事件驱动：Linux 用 inotify、Windows 用 `ReadDirectoryChangesW`、
//! macOS 用 kqueue 的 `EVFILT_VNODE`。拿不到原生监听（目录打不开、平台没有
//! 对应 API）时降级到 mtime 变化检测——那是文档化的兜底，不是常态。
//! 无新依赖：`windows-sys` 与 `libc` 本来就在 GUI 的依赖树里。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qcode_ui_bridge::{KernelPort, SessionTarget};

/// 任务表文件名（与 `SessionStore::schedule_tasks_path` 一致）。
const TASKS_JSON: &str = "tasks.json";

/// 防抖窗口：一拍里可能连续写多次（多任务确认、推进、回执），合并成一次通知，
/// 避免高频写时反复读盘解析。
const DEBOUNCE: Duration = Duration::from_millis(200);

/// 在专用线程里监听任务表目录。UI 进程在，监听就在；进程退出线程随进程消失。
pub(crate) fn spawn_schedule_watcher(port: &KernelPort, data_dir: &Path) {
    let schedule_dir = data_dir.join("schedule");
    let port = port.clone();
    std::thread::Builder::new()
        .name("schedule-fs-watch".into())
        .spawn(move || run(&schedule_dir, port))
        .ok();
}

/// 防抖后的通知器。`change` 可由任意平台后端调用，且可从任意线程调用。
struct Notifier {
    /// 是否已经有一发「延迟通知」在途。窗口内的后续写合并进那一发。
    pending: Arc<AtomicBool>,
    port: KernelPort,
}

impl Notifier {
    fn change(&self) {
        if self.pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let pending = self.pending.clone();
        let port = self.port.clone();
        std::thread::spawn(move || {
            std::thread::sleep(DEBOUNCE);
            pending.store(false, Ordering::SeqCst);
            // 与驱动自己的 `Host::changed` 走同一个事件、同一条刷新路径。
            port.tagged(SessionTarget::Draft).send_schedule_changed();
        });
    }
}

fn run(dir: &Path, port: KernelPort) {
    // 首次启动时任务表可能还没落盘过（目录不存在）：先建，监听对象才稳定。
    let _ = std::fs::create_dir_all(dir);
    let notifier = Notifier {
        pending: Arc::new(AtomicBool::new(false)),
        port,
    };
    if backend::watch(dir, &notifier).is_err() {
        tracing::warn!(
            dir = %dir.display(),
            "schedule fs-watch unavailable; falling back to mtime polling"
        );
        let _ = mtime_backend::watch(dir, &notifier);
    }
}

#[cfg(target_os = "linux")]
mod linux_backend {
    use super::*;
    use std::io;
    use std::os::unix::ffi::OsStrExt as _;

    use libc::{
        EINTR, IN_CLOEXEC, IN_CLOSE_WRITE, IN_CREATE, IN_MODIFY, IN_MOVED_TO, IN_NONBLOCK, POLLIN,
        inotify_add_watch, inotify_event, inotify_init1, poll, pollfd,
    };

    /// 阻塞监听 `dir`，`tasks.json` 每次被写入/替换时调 `change.change()`。
    pub(super) fn watch(dir: &Path, change: &Notifier) -> io::Result<()> {
        let fd = unsafe { inotify_init1(IN_CLOEXEC | IN_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let cdir = cstring(dir)?;
        let wd = unsafe {
            inotify_add_watch(
                fd,
                cdir.as_ptr(),
                IN_CREATE | IN_MOVED_TO | IN_CLOSE_WRITE | IN_MODIFY,
            )
        };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = [0u8; 64 * 1024];
        loop {
            // 阻塞等可读：事件驱动，空闲零 CPU。
            let mut fds = [pollfd {
                fd,
                events: POLLIN,
                revents: 0,
            }];
            let ready = unsafe { poll(fds.as_mut_ptr(), 1, -1) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(EINTR) {
                    continue;
                }
                return Err(error);
            }
            let n = unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut _, buffer.len()) };
            if n <= 0 {
                continue;
            }
            let mut offset = 0usize;
            while offset + size_of::<inotify_event>() <= n as usize {
                let event = unsafe {
                    std::ptr::read_unaligned(buffer.as_ptr().add(offset) as *const inotify_event)
                };
                if event.len > 0 {
                    let name = unsafe {
                        std::ffi::CStr::from_ptr(
                            buffer.as_ptr().add(offset + size_of::<inotify_event>()) as *const i8,
                        )
                    };
                    if name.to_bytes() == TASKS_JSON.as_bytes()
                        && event.mask & (IN_CREATE | IN_MOVED_TO | IN_CLOSE_WRITE | IN_MODIFY) != 0
                    {
                        change.change();
                    }
                }
                if event.len == 0 {
                    break;
                }
                offset += size_of::<inotify_event>() + event.len as usize;
            }
        }
    }

    fn cstring(path: &Path) -> io::Result<std::ffi::CString> {
        std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
    }
}

#[cfg(target_os = "macos")]
mod macos_backend {
    use super::*;
    use std::io;

    use libc::{
        EV_ADD, EV_CLEAR, EV_SET, EVFILT_VNODE, NOTE_DELETE, NOTE_RENAME, NOTE_WRITE, O_DIRECTORY,
        O_RDONLY, kevent, kqueue,
    };

    /// 阻塞监听 `dir`（目录内容变化：rename 进出、写入），触发即 `change.change()`。
    pub(super) fn watch(dir: &Path, change: &Notifier) -> io::Result<()> {
        let cdir = std::ffi::CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let dir_fd = unsafe { libc::open(cdir.as_ptr(), O_RDONLY | O_DIRECTORY) };
        if dir_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let kq = unsafe { kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut event: libc::Kevent = std::mem::zeroed();
        EV_SET(
            &mut event,
            dir_fd as usize,
            EVFILT_VNODE,
            EV_ADD | EV_CLEAR,
            NOTE_WRITE | NOTE_RENAME | NOTE_DELETE,
            0,
            std::ptr::null_mut(),
        );
        if unsafe { kevent(kq, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        loop {
            let mut out: libc::Kevent = std::mem::zeroed();
            let n = unsafe { kevent(kq, std::ptr::null(), 0, &mut out, 1, std::ptr::null()) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n > 0 {
                change.change();
            }
        }
    }
}

#[cfg(windows)]
mod windows_backend {
    use super::*;
    use std::io;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_NOTIFY_ENUM_DIR, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_RENAMED_NEW_NAME,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, ReadDirectoryChangesW,
    };

    /// `FILE_NOTIFY_INFORMATION` 链里每个条目的定长头（名字是变长的 UTF-16 尾巴）。
    #[repr(C)]
    struct NotifyHeader {
        next: u32,
        action: u32,
        name_len: u32,
    }

    struct DirHandle(HANDLE);
    impl Drop for DirHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// 阻塞监听 `dir`，`tasks.json` 被新增/修改/rename 覆盖时调 `change.change()`。
    pub(super) fn watch(dir: &Path, change: &Notifier) -> io::Result<()> {
        let wide: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = DirHandle(handle);
        // `FILE_NOTIFY_INFORMATION` 需要 8 字节对齐；用 4 字节对齐的分配，把头拷出来。
        let mut buffer = vec![0u32; 16 * 1024];
        loop {
            let mut returned: u32 = 0;
            let ok = unsafe {
                ReadDirectoryChangesW(
                    handle.0,
                    buffer.as_mut_ptr() as *mut _,
                    (buffer.len() * 4) as u32,
                    0,
                    FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE,
                    &mut returned,
                    std::ptr::null_mut(),
                    None,
                )
            };
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR as i32) {
                    // 缓冲太小，目录事件漏了：扩大后重读。
                    buffer.resize(buffer.len() * 2, 0);
                    continue;
                }
                return Err(error);
            }
            let mut offset = 0usize;
            // 所有偏移都是字节偏移，所以先取字节指针，再按字节推进。
            let base = buffer.as_ptr() as *const u8;
            while offset + size_of::<NotifyHeader>() <= (returned as usize) {
                let header =
                    unsafe { std::ptr::read_unaligned(base.add(offset) as *const NotifyHeader) };
                let name_offset = offset + size_of::<NotifyHeader>();
                let name_units = (header.name_len as usize) / 2;
                if name_units > 0 && name_offset + name_units * 2 <= returned as usize {
                    let name = unsafe {
                        String::from_utf16_lossy(std::slice::from_raw_parts(
                            base.add(name_offset) as *const u16,
                            name_units,
                        ))
                    };
                    if matches!(
                        header.action,
                        FILE_ACTION_ADDED | FILE_ACTION_MODIFIED | FILE_ACTION_RENAMED_NEW_NAME
                    ) && name == TASKS_JSON
                    {
                        change.change();
                    }
                }
                if header.next == 0 {
                    break;
                }
                offset += header.next as usize;
            }
        }
    }
}

/// mtime 变化检测：原生监听不可用时的兜底（也覆盖没有对应 API 的平台）。
mod mtime_backend {
    use super::*;
    use std::time::SystemTime;

    /// 永不返回；每 ~1s 比较一次 `tasks.json` 的 mtime，变了调 `change.change()`。
    pub(super) fn watch(dir: &Path, change: &Notifier) -> std::io::Result<()> {
        let file = dir.join(TASKS_JSON);
        let mut last = modified(&file);
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            let now = modified(&file);
            if now != last {
                last = now;
                change.change();
            }
        }
    }

    fn modified(path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }
}

#[cfg(target_os = "linux")]
use linux_backend as backend;
#[cfg(target_os = "macos")]
use macos_backend as backend;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
use mtime_backend as backend;
#[cfg(windows)]
use windows_backend as backend;
