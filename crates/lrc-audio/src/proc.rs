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

/// 本机已知的 ffmpeg 安装路径（与 lrc-video 的 KNOWN_FFMPEG 保持一致）。
pub(crate) const KNOWN_FFMPEG: &str =
    r"C:\Program Files\ffmpeg-8.0.1-essentials_build\bin\ffmpeg.exe";

/// 环境变量名（与 lrc-video 一致）。
pub(crate) const FFMPEG_ENV: &str = "LRC_FFMPEG";

/// 解析逻辑本体（纯函数，方便单测）：显式参数 > 环境变量 > 已知安装路径。
///
/// 三步都拿不到返回 None，调用方用 "ffmpeg" 交给 PATH。
fn resolve_from(
    configured: Option<&str>,
    from_env: Option<&str>,
    known: Option<&str>,
) -> Option<String> {
    for candidate in [configured, from_env] {
        if let Some(p) = candidate {
            let p = p.trim();
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    known.map(str::to_string)
}

/// 找 ffmpeg：显式参数 > 环境变量 LRC_FFMPEG > 已知安装路径 >（调用方）PATH。
///
/// 与 lrc-video 的解析顺序完全一致；前三步都没有时返回 None。
pub(crate) fn resolve_ffmpeg(configured: Option<&str>) -> Option<String> {
    let from_env = std::env::var(FFMPEG_ENV).ok();
    let known = if std::path::Path::new(KNOWN_FFMPEG).is_file() {
        Some(KNOWN_FFMPEG)
    } else {
        None
    };
    resolve_from(configured, from_env.as_deref(), known)
}

/// 取一个能直接交给 Command::new 的路径：解析结果，否则用 PATH 上的 ffmpeg。
pub(crate) fn ffmpeg_command(configured: Option<&str>) -> String {
    resolve_ffmpeg(configured).unwrap_or_else(|| String::from("ffmpeg"))
}

#[cfg(test)]
pub(crate) static FFMPEG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        FFMPEG_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn explicit_argument_wins_over_everything() {
        assert_eq!(
            resolve_from(Some(r"D:\a\ffmpeg.exe"), Some(r"D:\b"), Some("c")).as_deref(),
            Some(r"D:\a\ffmpeg.exe")
        );
    }

    #[test]
    fn env_beats_known_path() {
        assert_eq!(
            resolve_from(None, Some(r"D:\env\ffmpeg.exe"), Some("known")).as_deref(),
            Some(r"D:\env\ffmpeg.exe")
        );
    }

    #[test]
    fn known_path_is_last_resort() {
        assert_eq!(
            resolve_from(None, None, Some("known")).as_deref(),
            Some("known")
        );
        // 三步都空 → None，调用方走 PATH
        assert_eq!(resolve_from(None, None, None), None);
        // 空串/纯空格当作没给
        assert_eq!(
            resolve_from(Some("   "), Some(""), Some("known")).as_deref(),
            Some("known")
        );
        assert_eq!(resolve_from(Some(""), None, None), None);
    }

    /// 设 LRC_FFMPEG → 解析出该路径；清掉 → 不再是它（回到已知路径或 None）。
    #[test]
    fn env_var_is_honored() {
        let _guard = lock();
        let saved = std::env::var(FFMPEG_ENV).ok();
        let probe = r"D:\tmp\ffmpeg-env-probe.exe";

        std::env::set_var(FFMPEG_ENV, probe);
        let with_env = resolve_ffmpeg(None);
        std::env::remove_var(FFMPEG_ENV);
        let without_env = resolve_ffmpeg(None);

        if let Some(p) = saved {
            std::env::set_var(FFMPEG_ENV, p);
        }

        assert_eq!(with_env.as_deref(), Some(probe), "设了环境变量就该用它");
        assert_ne!(
            without_env.as_deref(),
            Some(probe),
            "清掉环境变量后不该再解析到它"
        );
        // 清掉之后要么回到已知安装路径，要么 None（交给 PATH）
        assert!(
            without_env.is_none() || without_env.as_deref() == Some(KNOWN_FFMPEG),
            "兜底只可能是已知路径或 None"
        );
    }

    #[test]
    fn explicit_path_is_used_for_command() {
        let _guard = lock();
        assert_eq!(ffmpeg_command(Some(r"D:\x\ffmpeg.exe")), r"D:\x\ffmpeg.exe");
    }
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
        self.resync_ogg();
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