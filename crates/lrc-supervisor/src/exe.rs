//! 可执行文件身份：把"跑的是哪个二进制"变成可比较的数据。

use std::path::Path;
use std::time::SystemTime;

use crate::status::ExeIdentity;

/// 读取文件身份；文件不在返回 None。
pub fn identify(path: &Path) -> Option<ExeIdentity> {
    let meta = std::fs::metadata(path).ok()?;
    Some(ExeIdentity {
        path: path.display().to_string(),
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// 两个身份是否是同一个构建。
///
/// 只看大小 + 修改时间：比 hash 便宜得多（host 有 2MB+），而"换了 exe 却没重启"
/// 这种问题用这两个字段就足够分辨了。
pub fn same_build(a: &ExeIdentity, b: &ExeIdentity) -> bool {
    a.len == b.len && a.modified == b.modified
}

/// 期望 vs 实际的比较结果，给前端直接展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// 一致
    Match,
    /// 跑着的是旧版本（磁盘上的更新）
    Stale,
    /// 拿不到信息
    Unknown,
}

pub fn freshness(expected: Option<&ExeIdentity>, running: Option<&ExeIdentity>) -> Freshness {
    match (expected, running) {
        (Some(e), Some(r)) => {
            if same_build(e, r) {
                Freshness::Match
            } else {
                Freshness::Stale
            }
        }
        _ => Freshness::Unknown,
    }
}

/// 把 SystemTime 格式化成便于人读的 UTC 字符串（不引 chrono）。
pub fn format_time(t: Option<SystemTime>) -> String {
    let Some(t) = t else {
        return String::from("-");
    };
    let Ok(d) = t.duration_since(SystemTime::UNIX_EPOCH) else {
        return String::from("-");
    };
    let secs = d.as_secs();
    // 只到分钟，够用：2026-11-26T17:35
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Howard Hinnant 的 days-from-civil 逆运算（无依赖，避免为格式化引 chrono）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ident(len: u64, secs: u64) -> ExeIdentity {
        ExeIdentity {
            path: String::from("x.exe"),
            len,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
        }
    }

    #[test]
    fn same_build_compares_len_and_mtime() {
        assert!(same_build(&ident(100, 10), &ident(100, 10)));
        assert!(!same_build(&ident(100, 10), &ident(101, 10)), "大小变了就是不同构建");
        assert!(!same_build(&ident(100, 10), &ident(100, 11)), "时间变了就是不同构建");
    }

    #[test]
    fn freshness_flags_stale_binary() {
        // P0 场景：磁盘上是新 exe，跑着的是旧的 → Stale，前端标红
        let disk = ident(200, 20);
        let running = ident(100, 10);
        assert_eq!(freshness(Some(&disk), Some(&running)), Freshness::Stale);
        assert_eq!(freshness(Some(&disk), Some(&disk)), Freshness::Match);
        assert_eq!(freshness(Some(&disk), None), Freshness::Unknown);
    }

    #[test]
    fn format_time_is_utc_iso_like() {
        // 2026-11-26T00:00:00Z = 1795651200
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_651_200);
        assert_eq!(format_time(Some(t)), "2026-11-26T00:00");
        assert_eq!(format_time(None), "-");
    }
}
