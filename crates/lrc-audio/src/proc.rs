//! ffmpeg 子进程封装：管道读写 + 非阻塞"有多少读多少" + 退出清理。

use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Pipes::PeekNamedPipe;

use crate::{AudioError, AudioResult};

/// 不弹控制台窗口。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 找 ffmpeg：显式覆盖 > 本机默认安装路径 > PATH。
pub(crate) fn resolve_ffmpeg(configured: Option<&str>) -> String {
    if let Some(p) = configured {
        if !p.trim().is_empty() {
            return p.to_string();
        }
    }
    const DEFAULT: &str = r"C:\Program Files\ffmpeg-8.0.1-essentials_build\bin\ffmpeg.exe";
    if std::path::Path::new(DEFAULT).is_file() {
        return DEFAULT.to_string();
    }
    "ffmpeg".to_string()
}

/// 一个 ffmpeg 子进程（stdin 灌数据、stdout 取数据）。
pub(crate) struct FfmpegChild {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    handle: HANDLE,
    /// 已经从管道读出来、还没被上层取走的字节
    pending: Vec<u8>,
}

impl FfmpegChild {
    pub fn spawn(ffmpeg: &str, args: &[String]) -> AudioResult<Self> {
        let mut cmd = Command::new(ffmpeg);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        let mut child = cmd
            .spawn()
            .map_err(|e| AudioError::Io(format!("启动 ffmpeg({ffmpeg}) 失败: {e}")))?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AudioError::Io("拿不到 ffmpeg stdout".to_string()))?;
        let handle = HANDLE(stdout.as_raw_handle() as *mut core::ffi::c_void);
        Ok(Self {
            child,
            stdin,
            stdout,
            handle,
            pending: Vec::with_capacity(16 * 1024),
        })
    }

    /// 子进程 pid（自测清理用）。
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn write(&mut self, data: &[u8]) -> AudioResult<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(AudioError::Io("ffmpeg stdin 已关闭".to_string()));
        };
        stdin
            .write_all(data)
            .map_err(|e| AudioError::Io(format!("写 ffmpeg stdin: {e}")))?;
        stdin
            .flush()
            .map_err(|e| AudioError::Io(format!("flush ffmpeg stdin: {e}")))?;
        Ok(())
    }

    /// 把管道里**已经到达**的字节读进 pending（不阻塞）。
    pub fn pump(&mut self) -> AudioResult<usize> {
        let mut avail: u32 = 0;
        unsafe { PeekNamedPipe(self.handle, None, 0, None, Some(&mut avail), None) }
            .map_err(|e| AudioError::Io(format!("PeekNamedPipe: {e}")))?;
        if avail == 0 {
            return Ok(0);
        }
        let n = avail as usize;
        let start = self.pending.len();
        self.pending.resize(start + n, 0);
        self.stdout
            .read_exact(&mut self.pending[start..])
            .map_err(|e| AudioError::Io(format!("读 ffmpeg stdout: {e}")))?;
        Ok(n)
    }

    /// 如果流里混了垃圾字节，丢到下一个 OggS 为止（只在编码侧用）。
    pub fn resync_ogg(&mut self) {
        if self.pending.len() >= 4 && &self.pending[0..4] != b"OggS" {
            if let Some(pos) = crate::ogg::find_next_page(&self.pending) {
                log::warn!("audio: ffmpeg 输出里混进 {pos} 字节垃圾，已丢弃");
                self.pending.drain(..pos);
            }
        }
    }

    /// 等到 pending 里能切出至少一页 Ogg，最多等 wait_ms。
    pub fn wait_page(&mut self, wait_ms: u32) -> AudioResult<()> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(wait_ms));
        loop {
            self.pump()?;
            self.resync_ogg();
            if crate::ogg::page_len(&self.pending).is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// 等到 pending 里至少有 at_least 字节，最多等 wait_ms。
    pub fn wait_bytes(&mut self, wait_ms: u32, at_least: usize) -> AudioResult<()> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(wait_ms));
        loop {
            self.pump()?;
            if self.pending.len() >= at_least || Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// 把 pending 里所有完整 Ogg 页搬进 out。
    pub fn take_pages(&mut self, out: &mut Vec<u8>) {
        loop {
            let Some(len) = crate::ogg::page_len(&self.pending) else {
                break;
            };
            out.extend_from_slice(&self.pending[..len]);
            self.pending.drain(..len);
        }
    }

    /// 把 pending 里所有字节搬进 out（解码侧 PCM 不需要分帧）。
    pub fn take_all(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.pending);
        self.pending.clear();
    }
}

impl Drop for FfmpegChild {
    fn drop(&mut self) {
        // 关 stdin 让 ffmpeg 看到 EOF 自己退；200ms 不退就杀，再回收，避免残留进程
        let stdin = self.stdin.take();
        drop(stdin);
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
    }
}
