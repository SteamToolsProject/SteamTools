//! DevTools pipe: 用继承的 fd 3/4 替代 TCP 调试端口.
//!
//! Chromium 的 `--remote-debugging-pipe` 从 **fd 3 读、往 fd 4 写** CDP 消息
//! (`\0` 分隔的裸 JSON, 没有 WebSocket 帧) — 已用本机 Edge 150 端到端实测.
//! Windows 上 fd≥3 通过 CRT 的 `STARTUPINFOW::lpReserved2` 块继承; libuv/Node
//! 正是靠这个给 Chromium 传 fd 3/4, Puppeteer 的 pipe 模式即建立在此之上.
//!
//! 换成 pipe 之后调试通道没有端口可扫、没有 URL 可连, 只有持有管道那一端的父
//! 进程能用. 这是 ADR 0010 里的 B 档.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows::Win32::Foundation::{
    SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, UpdateProcThreadAttribute,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTUPINFOEXW, STARTUPINFOW,
};

/// CRT fd 标志: 该 fd 已打开.
const FOPEN: u8 = 0x01;
/// CRT fd 标志: 该 fd 是管道.
const FPIPE: u8 = 0x08;

/// Chromium 读 CDP 的 fd.
pub const CHILD_READ_FD: usize = 3;
/// Chromium 写 CDP 的 fd.
pub const CHILD_WRITE_FD: usize = 4;
/// fd 表要覆盖 0..=4, 前三个留空.
const FD_TABLE_LEN: usize = 5;

/// `dwCreationFlags` 位: `lpStartupInfo` 实际指向 `STARTUPINFOEXW`.
///
/// 少了这个位, 我们挂上去的句柄白名单会被静默忽略.
pub const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;

/// x64 `STARTUPINFOW` 的字节大小.
///
/// 读别人传来的 `lpStartupInfo` 前拿它做一次合理性检查.
pub const STARTUPINFOW_SIZE: usize = std::mem::size_of::<STARTUPINFOW>();

/// 构造 CRT 的 fd 继承块 (`STARTUPINFOW::lpReserved2`).
///
/// 布局 (MSVCRT/UCRT 私有约定, 但已是事实标准):
///
/// ```text
/// [u32 count][u8 flags * count][usize handle * count]
/// ```
///
/// `None` 表示该 fd 未打开 — 写 `INVALID_HANDLE_VALUE`, 标志为 0.
pub fn crt_fd_block(fds: &[Option<usize>]) -> Vec<u8> {
    let count = fds.len();
    let mut out = Vec::with_capacity(4 + count * (1 + std::mem::size_of::<usize>()));
    // 每个 fd 至少占 9 字节, 所以 slice 长度不可能溢出 u32.
    out.extend_from_slice(&(count as u32).to_ne_bytes());
    for fd in fds {
        out.push(if fd.is_some() { FOPEN | FPIPE } else { 0 });
    }
    for fd in fds {
        // 未占用的槽必须是 INVALID_HANDLE_VALUE, 不能是 0 — 0 是合法句柄值.
        let raw = fd.unwrap_or(INVALID_HANDLE_VALUE.0 as usize);
        out.extend_from_slice(&raw.to_ne_bytes());
    }
    out
}

/// 父进程这一侧的 CDP 管道端点.
#[derive(Debug)]
pub struct DevToolsPipe {
    /// 我们写 → 子进程从 fd 3 读.
    writer: OwnedHandle,
    /// 子进程往 fd 4 写 → 我们读.
    reader: OwnedHandle,
}

impl DevToolsPipe {
    /// 写一条 CDP 消息 (自动补 `\0` 结束符).
    pub fn send(&self, msg: &str) -> io::Result<()> {
        let mut buf = Vec::with_capacity(msg.len() + 1);
        buf.extend_from_slice(msg.as_bytes());
        buf.push(0);
        let mut off = 0usize;
        while off < buf.len() {
            let mut written = 0u32;
            // SAFETY: 句柄由 OwnedHandle 保有且未关闭; 缓冲与长度来自同一个切片.
            unsafe {
                WriteFile(
                    HANDLE(self.writer.as_raw_handle()),
                    Some(&buf[off..]),
                    Some(&mut written),
                    None,
                )
            }
            .map_err(io::Error::other)?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "devtools pipe closed",
                ));
            }
            off += written as usize;
        }
        Ok(())
    }

    /// 读若干字节; 返回 0 表示对端已关闭.
    ///
    /// 匿名管道没有超时, 调用方要自己保证只在有数据时读 (见 `cdp_pipe` 的读线程).
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut got = 0u32;
        // SAFETY: 句柄由 OwnedHandle 保有且未关闭; 缓冲与长度来自同一个切片.
        unsafe {
            ReadFile(
                HANDLE(self.reader.as_raw_handle()),
                Some(buf),
                Some(&mut got),
                None,
            )
        }
        .map_err(io::Error::other)?;
        Ok(got as usize)
    }
}

/// 待交给 `CreateProcessW` 的一次性参数包.
///
/// 里面的缓冲被 `STARTUPINFOEXW` 按指针引用, **必须活到 `CreateProcessW` 返回**,
/// 所以整包一起持有, 不拆开传.
pub struct ChildPipeLaunch {
    startup: Box<STARTUPINFOEXW>,
    /// 属性列表按裸指针引用它 — 只为保命, 不读.
    _attr_buf: Vec<u8>,
    /// 属性列表按裸指针引用它 — 只为保命, 不读.
    _inherit_handles: Box<[HANDLE; 2]>,
    /// `lpReserved2` 指向它 — 只为保命, 不读.
    _crt_block: Vec<u8>,
    /// 交给子进程的两端; `CreateProcessW` 返回后即可丢弃.
    _child_read: OwnedHandle,
    _child_write: OwnedHandle,
}

impl ChildPipeLaunch {
    /// 传给 `CreateProcessW` 的 `lpStartupInfo`.
    ///
    /// 必须同时给 `dwCreationFlags` 加上 [`EXTENDED_STARTUPINFO_PRESENT`],
    /// 并把 `bInheritHandles` 设为 `TRUE`, 否则句柄白名单会被忽略.
    ///
    /// 移动 `ChildPipeLaunch` 是安全的: 被引用的 `Vec`/`Box` 缓冲都在堆上,
    /// 移动结构体不会挪动它们.
    pub fn startup_info(&self) -> *const c_void {
        std::ptr::from_ref(&*self.startup).cast()
    }
}

impl Drop for ChildPipeLaunch {
    fn drop(&mut self) {
        if !self.startup.lpAttributeList.0.is_null() {
            // SAFETY: 该列表由 build_handle_list_attributes 初始化过, 且只销毁一次
            // (Drop 只跑一次). 字段在 Drop::drop 之后才释放, 销毁时缓冲仍然有效.
            unsafe { DeleteProcThreadAttributeList(self.startup.lpAttributeList) };
        }
    }
}

/// 建一对 CDP 管道, 并备好把它们作为 fd 3/4 传下去的 `CreateProcessW` 参数.
///
/// `template` 是调用方原本要传的 `STARTUPINFOW`; 我们在它基础上加 fd 表与句柄
/// 白名单. 返回 `None` 表示这次别注入 (调用方应原样放行).
///
/// # Safety
/// `template` 须为合法的 `STARTUPINFOW` 指针, 或空.
pub unsafe fn prepare_devtools_pipe(
    template: *const c_void,
) -> Option<(DevToolsPipe, ChildPipeLaunch)> {
    // 两条单向匿名管道: 一条我们写子进程读, 一条子进程写我们读.
    let (child_read, our_write) = create_pipe_pair()?;
    let (our_read, child_write) = create_pipe_pair()?;
    // 我们留着的两端不给子进程 — 句柄白名单已经限定了, 这里再清一道.
    set_inheritable(&our_write, false)?;
    set_inheritable(&our_read, false)?;
    set_inheritable(&child_read, true)?;
    set_inheritable(&child_write, true)?;

    let mut startup = Box::new(STARTUPINFOEXW::default());
    // 保留调用方原本的字段 (lpDesktop / 窗口位置等), 只加我们要的.
    if !template.is_null() {
        let src = template.cast::<STARTUPINFOW>();
        // SAFETY: 由调用方保证 template 指向合法 STARTUPINFOW (见 # Safety);
        // 用 read_unaligned 是因为来源指针的对齐不受我们控制.
        startup.StartupInfo = std::ptr::read_unaligned(src);
    }
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;

    // fd 0/1/2 保持未打开: 实测 Steam 自己也没用 (cbReserved2=0).
    let mut fds = [None; FD_TABLE_LEN];
    fds[CHILD_READ_FD] = Some(child_read.as_raw_handle() as usize);
    fds[CHILD_WRITE_FD] = Some(child_write.as_raw_handle() as usize);
    let crt_block = crt_fd_block(&fds);
    startup.StartupInfo.cbReserved2 = u16::try_from(crt_block.len()).ok()?;
    startup.StartupInfo.lpReserved2 = crt_block.as_ptr().cast_mut();

    // 句柄白名单: 子进程**只**继承这两个, 其余可继承句柄一概不给.
    // 这就是为什么把 bInheritHandles 打开不构成泄漏.
    let inherit_handles = Box::new([
        HANDLE(child_read.as_raw_handle()),
        HANDLE(child_write.as_raw_handle()),
    ]);
    let attr_buf = build_handle_list_attributes(&mut startup, &inherit_handles)?;

    Some((
        DevToolsPipe {
            writer: our_write,
            reader: our_read,
        },
        ChildPipeLaunch {
            startup,
            _attr_buf: attr_buf,
            _inherit_handles: inherit_handles,
            _crt_block: crt_block,
            _child_read: child_read,
            _child_write: child_write,
        },
    ))
}

/// 分配并填好 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`; 返回承载它的缓冲.
fn build_handle_list_attributes(
    startup: &mut STARTUPINFOEXW,
    handles: &[HANDLE; 2],
) -> Option<Vec<u8>> {
    let mut size = 0usize;
    // 按约定第一次调用必然失败 (ERROR_INSUFFICIENT_BUFFER), 只为问出需要多大.
    // SAFETY: 传空列表问尺寸是这个 API 的既定用法.
    let _ = unsafe {
        InitializeProcThreadAttributeList(LPPROC_THREAD_ATTRIBUTE_LIST::default(), 1, 0, &mut size)
    };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr().cast());
    // SAFETY: buf 恰好是上一步问出的大小, 且在返回后仍由调用方持有.
    unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) }.ok()?;
    // SAFETY: list 已初始化; handles 是长度为 2 的数组, 由调用方保活到进程创建完成.
    unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(handles.as_ptr().cast()),
            std::mem::size_of_val(handles),
            None,
            None,
        )
    }
    .ok()?;
    startup.lpAttributeList = list;
    Some(buf)
}

/// `CreatePipe`, 两端都先建成可继承的.
fn create_pipe_pair() -> Option<(OwnedHandle, OwnedHandle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    // SAFETY: 两个出参都是栈上的合法 HANDLE 槽; sa 在调用期间有效.
    unsafe { CreatePipe(&mut read, &mut write, Some(&sa), 0) }.ok()?;
    // SAFETY: CreatePipe 成功返回的两个句柄都归我们所有.
    unsafe {
        Some((
            OwnedHandle::from_raw_handle(read.0),
            OwnedHandle::from_raw_handle(write.0),
        ))
    }
}

fn set_inheritable(h: &OwnedHandle, on: bool) -> Option<()> {
    // SAFETY: 句柄由 OwnedHandle 保有, 调用期间不会被关闭.
    unsafe {
        SetHandleInformation(
            HANDLE(h.as_raw_handle()),
            HANDLE_FLAG_INHERIT.0,
            if on {
                HANDLE_FLAG_INHERIT
            } else {
                HANDLE_FLAGS(0)
            },
        )
    }
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_header_is_the_fd_count() {
        let b = crt_fd_block(&[None, None, None]);
        assert_eq!(u32::from_ne_bytes(b[..4].try_into().unwrap()), 3);
    }

    #[test]
    fn block_length_matches_layout() {
        let b = crt_fd_block(&[None; FD_TABLE_LEN]);
        // 4 字节计数 + 每 fd 一个标志字节 + 每 fd 一个句柄.
        assert_eq!(
            b.len(),
            4 + FD_TABLE_LEN * (1 + std::mem::size_of::<usize>())
        );
    }

    #[test]
    fn open_slots_are_flagged_as_pipes() {
        let b = crt_fd_block(&[None, Some(0x1234)]);
        assert_eq!(b[4], 0, "未占用的 fd 不该带标志");
        assert_eq!(b[5], FOPEN | FPIPE);
    }

    #[test]
    fn unused_slots_hold_invalid_handle_not_zero() {
        let b = crt_fd_block(&[None]);
        let h = usize::from_ne_bytes(b[5..5 + std::mem::size_of::<usize>()].try_into().unwrap());
        // 0 是合法句柄值, 空槽必须写 INVALID_HANDLE_VALUE.
        assert_eq!(h, INVALID_HANDLE_VALUE.0 as usize);
        assert_ne!(h, 0);
    }

    #[test]
    fn handles_follow_the_flag_array() {
        let b = crt_fd_block(&[Some(0xAA), Some(0xBB)]);
        let off = 4 + 2;
        let sz = std::mem::size_of::<usize>();
        assert_eq!(
            usize::from_ne_bytes(b[off..off + sz].try_into().unwrap()),
            0xAA
        );
        assert_eq!(
            usize::from_ne_bytes(b[off + sz..off + 2 * sz].try_into().unwrap()),
            0xBB
        );
    }

    #[test]
    fn chromium_fds_land_in_the_right_slots() {
        let mut fds = [None; FD_TABLE_LEN];
        fds[CHILD_READ_FD] = Some(0x11);
        fds[CHILD_WRITE_FD] = Some(0x22);
        let b = crt_fd_block(&fds);
        // fd 0/1/2 未打开, 3/4 是管道.
        assert_eq!(
            &b[4..4 + FD_TABLE_LEN],
            &[0, 0, 0, FOPEN | FPIPE, FOPEN | FPIPE]
        );
    }

    #[test]
    fn pipe_pair_round_trips() {
        let (a, b) = create_pipe_pair().expect("CreatePipe");
        let pipe = DevToolsPipe {
            writer: b,
            reader: a,
        };
        pipe.send(r#"{"id":1}"#).unwrap();
        let mut buf = [0u8; 32];
        let n = pipe.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"{\"id\":1}\0");
    }
}
