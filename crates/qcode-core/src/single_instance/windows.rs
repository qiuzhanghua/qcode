//! Windows 端：`LockFileEx` + 命名管道。
//!
//! 通道不能是文件：Windows 没有「一个文件路径就是一条 UDS」那套东西，命名
//! 管道是同一件事的原生形状。名字里带 data dir 的哈希，于是「作用域 = data
//! dir」这条（ADR-0083 §2）与 Unix 是同一个语义，不需要额外状态。

use std::io::{self, BufRead, BufReader, Write};
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, PIPE_ACCESS_DUPLEX, ReadFile,
    WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use super::{Request, request_line};

/// 通道在 data dir 里的「文件名」。Windows 上它不是真文件，只是给
/// [`Paths`](super::Paths) 一个稳定的落脚点（主要是日志里能显示个东西）。
pub(super) const CHANNEL_EXT: &str = "pipe";

/// `ERROR_PIPE_CONNECTED`（535）：客户端在我们开始等之前就连上了。不是失败。
const ERROR_PIPE_CONNECTED: u32 = 535;
/// `ERROR_LOCK_VIOLATION`（33）：别人拿着那把锁。
const ERROR_LOCK_VIOLATION: u32 = 33;

/// 哈希只要求「同机稳定、不同目录不同」，不要求抗攻击：名字泄露也没什么可
/// 攻击的——真正的互斥是那把锁。忽略大小写，因为 Windows 路径不分大小写。
fn hash(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.to_string_lossy().to_lowercase().hash(&mut hasher);
    hasher.finish()
}

pub(super) fn channel_address(paths: &super::Paths) -> String {
    format!(
        r"\\.\pipe\qcode-{}-{:016x}",
        paths.scope().prefix(),
        hash(&paths.data_dir)
    )
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

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
    // SAFETY: `overlapped` 是有效的栈上 OVERLAPPED，同步用法下内核只看它的
    // 偏移量字段（全零 = 从第 0 字节开始）。锁一个字节就够表达「独占」。
    let mut overlapped = unsafe { std::mem::zeroed() };
    let handle: HANDLE = file.as_raw_handle() as HANDLE;
    let rc = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if rc != 0 {
        return Ok(Some(LockGuard { _file: file }));
    }
    let code = unsafe { GetLastError() };
    if code == ERROR_LOCK_VIOLATION {
        return Ok(None);
    }
    Err(io::Error::from_raw_os_error(code as i32))
}

pub(super) fn try_request(paths: &super::Paths, request: Request) -> io::Result<bool> {
    let name = channel_address(paths);
    // std 的 `OpenOptions` 走 `CreateFileW`，它认得 `\\.\pipe\...`。
    let mut stream = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&name)
    {
        Ok(file) => file,
        Err(error) => {
            let code = error.raw_os_error().unwrap_or(0) as u32;
            // 管道不存在 = 没有活实例；实例都忙着 = 有人在听但此刻接不了，
            // 对只有一件事的协议来说同样是「这次没送出去」。
            if code == ERROR_FILE_NOT_FOUND || code == ERROR_PIPE_BUSY {
                return Ok(false);
            }
            return Err(error);
        }
    };
    stream.write_all(request_line(request).as_bytes())?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let ok = serde_json::from_str::<serde_json::Value>(line.trim())
        .ok()
        .and_then(|value| value.get("ok").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    if ok {
        Ok(true)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected reply from {name}: {line:?}"),
        ))
    }
}

pub(super) struct Listener {
    name: String,
}

pub(super) fn listen(paths: &super::Paths) -> io::Result<Listener> {
    // 命名管道不需要「清掉残留文件」那一步：进程一死，管道实例就没了。
    Ok(Listener {
        name: channel_address(paths),
    })
}

/// 把命名管道句柄包成 `Write`，让回调与 unix 后端看到同一个接口。
///
/// 只实现 `write`/`flush`：协议是「一行 JSON 的应答」，不需要 `read`、不需要
/// seek。
struct PipeReply {
    handle: HANDLE,
}

impl std::io::Write for PipeReply {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut written = 0u32;
        // SAFETY: `handle` 由调用方保证在这个函数返回前有效（它就是刚读到请求
        // 的那个句柄）；`buf` 是本进程的字节切片。
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // `WriteFile` 已经把字节交给管道了，没有用户态缓冲可刷。
        Ok(())
    }
}

impl Listener {
    pub(super) fn accept_loop<F: Fn(Request, &mut dyn std::io::Write) -> bool>(
        self,
        on_request: F,
    ) {
        let wide_name = wide(&self.name);
        loop {
            // SAFETY: 名字是以 NUL 结尾的 UTF-16；其余参数都是常量，
            // 默认安全属性（`null`）表示「当前用户的默认 DACL」。
            let handle = unsafe {
                CreateNamedPipeW(
                    wide_name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    4096,
                    4096,
                    0,
                    std::ptr::null(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                // 建不出管道就没什么可做的了：主流程已经起来了，这里安静退出
                // 比让线程空转好。
                return;
            }
            // 阻塞等一个客户端；这就是这个线程唯一的工作。
            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            if connected == 0 {
                let code = unsafe { GetLastError() };
                if code != ERROR_PIPE_CONNECTED {
                    unsafe { CloseHandle(handle) };
                    continue;
                }
            }

            let mut buffer = [0u8; 256];
            let mut read = 0u32;
            // SAFETY: buffer 是 256 字节的有效可写区域，`read` 是有效出参。
            let ok = unsafe {
                ReadFile(
                    handle,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 && read > 0 {
                let line = String::from_utf8_lossy(&buffer[..read as usize]);
                if let Some(request) = Request::parse(&line) {
                    // 应答由回调写（与 unix 后端同一条规则：`Stop` 必须先确认再
                    // 退出）。这里给的是一个最小的 `Write`，把原生句柄包起来，
                    // 于是回调两端看到的是同一个接口。
                    let mut reply = PipeReply { handle };
                    let keep_going = on_request(request, &mut reply);
                    if !keep_going {
                        // 不再接活：与 unix 分支一样 quiet 退出。
                        unsafe {
                            DisconnectNamedPipe(handle);
                            CloseHandle(handle);
                        }
                        break;
                    }
                }
            }
            // 一条连接一个请求：协议只有一件事，处理完就断。
            unsafe {
                DisconnectNamedPipe(handle);
                CloseHandle(handle);
            }
        }
    }
}
