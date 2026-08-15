//! 极简文件日志：按"日志日"（凌晨 4 点为日界）分文件，追加写入
//! ~/.dsh/launcher-YYYY-MM-DD.log，仅保留最近 3 天，过期文件自动清理。
//!
//! 守护程序无控制台窗口，故障诊断依赖此日志。
//! 记录点：watchdog 端口状态与拉起、启动流程起止、Job 操作成败、stop 脚本执行。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// 日志保留天数（含今天）
const KEEP_DAYS: i64 = 3;

/// 上次清理时所在的日志日：避免每次写入都扫描目录，一天只清理一次
static LAST_CLEANUP: Mutex<Option<(i64, u32, u32)>> = Mutex::new(None);

/// 日志目录：DSHLAUNCHER_LOG_DIR 覆盖（沙箱/CI 调试钩子），否则 %USERPROFILE%\.dsh
fn log_base_dir() -> PathBuf {
    match std::env::var("DSHLAUNCHER_LOG_DIR") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".dsh")
        }
    }
}

/// 实例后缀（DSHLAUNCHER_INSTANCE，测试实例隔离日志文件）
fn instance_suffix() -> String {
    match std::env::var("DSHLAUNCHER_INSTANCE") {
        Ok(s) if !s.is_empty() => format!("{s}-"),
        _ => String::new(),
    }
}

/// 当前日志文件路径：{base}/launcher[-{instance}-]{YYYY-MM-DD}.log
fn log_path() -> PathBuf {
    let (y, m, d) = current_log_day();
    log_base_dir().join(format!(
        "launcher-{}{:04}-{:02}-{:02}.log",
        instance_suffix(),
        y,
        m,
        d
    ))
}

/// 本地时间戳 YYYY-MM-DD HH:MM:SS
fn timestamp() -> String {
    unsafe {
        let mut st: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut st);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
}

/// 民用日期 → 自 1970-01-01 起的天数（Howard Hinnant 算法，正确处理闰年）
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let (m, d) = (m as i64, d as i64);
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 天数 → 民用日期（Howard Hinnant 算法）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// 当前时刻归属的日志日（凌晨 4 点前归前一天）
fn current_log_day() -> (i64, u32, u32) {
    unsafe {
        let mut st: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut st);
        log_day_of(&st)
    }
}

/// 计算 SYSTEMTIME 归属的日志日（凌晨 4 点为日界）
fn log_day_of(st: &SYSTEMTIME) -> (i64, u32, u32) {
    let days = days_from_civil(st.wYear as i64, st.wMonth as u32, st.wDay as u32);
    let adjusted = if st.wHour < 4 { days - 1 } else { days };
    civil_from_days(adjusted)
}

/// 删除超过保留天数的日志文件（按文件名中的日期，仅匹配本实例前缀）
fn cleanup_old_logs() {
    let today = current_log_day();
    let mut last = LAST_CLEANUP.lock().unwrap_or_else(|p| p.into_inner());
    if *last == Some(today) {
        return;
    }
    *last = Some(today);

    let today_days = days_from_civil(today.0, today.1, today.2);
    let cutoff = today_days - (KEEP_DAYS - 1); // 今天及之前 2 天保留
    let prefix = format!("launcher-{}", instance_suffix());
    let Ok(entries) = std::fs::read_dir(log_base_dir()) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // prefix 已含尾部连字符（launcher- 或 launcher-test-），直接剥离后应为日期
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(date_part) = rest.strip_suffix(".log") else {
            continue;
        };
        let parts: Vec<&str> = date_part.split('-').collect();
        if parts.len() != 3 {
            continue;
        }
        let (Ok(y), Ok(m), Ok(d)) = (
            parts[0].parse::<i64>(),
            parts[1].parse::<u32>(),
            parts[2].parse::<u32>(),
        ) else {
            continue;
        };
        if days_from_civil(y, m, d) < cutoff && std::fs::remove_file(entry.path()).is_ok() {
            eprintln!("[log] 清理过期日志: {}", entry.path().display());
        }
    }
}

fn write(level: &str, msg: &str) {
    cleanup_old_logs();
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!("{} [{}] {}\n", timestamp(), level, msg);
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            let _ = f.write_all(line.as_bytes());
        }
        Err(e) => {
            eprintln!("[log] 打开日志文件失败 {}: {e}", path.display());
        }
    }
}

/// 记录一条 INFO 日志
pub fn info(msg: &str) {
    write("INFO", msg);
}

/// 记录一条 WARN 日志
pub fn warn(msg: &str) {
    write("WARN", msg);
}

/// 记录一条 FAIL 日志（4 字符标签，与 INFO/WARN 对齐）
pub fn error(msg: &str) {
    write("FAIL", msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 凌晨 4 点日界与跨月/跨年/闰年边界
    #[test]
    fn log_day_boundary() {
        let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
        st.wYear = 2026;
        st.wMonth = 8;
        st.wDay = 15;
        st.wHour = 3;
        st.wMinute = 59;
        assert_eq!(log_day_of(&st), (2026, 8, 14), "03:59 应归前一天");

        st.wHour = 4;
        st.wMinute = 0;
        assert_eq!(log_day_of(&st), (2026, 8, 15), "04:00 应归当天");

        // 非闰年 3 月 1 日 03:00 → 2 月 28 日
        st.wYear = 2026;
        st.wMonth = 3;
        st.wDay = 1;
        st.wHour = 3;
        assert_eq!(log_day_of(&st), (2026, 2, 28), "非闰年 2 月应为 28 天");

        // 闰年 3 月 1 日 03:00 → 2 月 29 日
        st.wYear = 2024;
        st.wMonth = 3;
        st.wDay = 1;
        st.wHour = 3;
        assert_eq!(log_day_of(&st), (2024, 2, 29), "闰年 2 月应为 29 天");

        // 跨年：1 月 1 日 03:00 → 上一年 12 月 31 日
        st.wYear = 2026;
        st.wMonth = 1;
        st.wDay = 1;
        st.wHour = 3;
        assert_eq!(log_day_of(&st), (2025, 12, 31), "跨年边界");

        // 世纪闰年：2000 年 3 月 1 日 03:00 → 2 月 29 日（2000 是闰年）
        st.wYear = 2000;
        st.wMonth = 3;
        st.wDay = 1;
        st.wHour = 3;
        assert_eq!(log_day_of(&st), (2000, 2, 29), "世纪闰年 2000");

        // 非世纪闰年：1900 年 3 月 1 日 03:00 → 2 月 28 日（1900 不是闰年）
        st.wYear = 1900;
        st.wMonth = 3;
        st.wDay = 1;
        st.wHour = 3;
        assert_eq!(log_day_of(&st), (1900, 2, 28), "世纪非闰年 1900");
    }

    /// 日志写入与文件创建（临时目录 + LOG_DIR 钩子，不依赖用户主目录权限）
    #[test]
    fn log_write_creates_file() {
        // 注意：set_var 修改进程环境变量，仅本测试使用且无其他测试并发读取该变量
        let tmp = std::env::temp_dir().join("dsh-launcher-log-test");
        let _ = std::fs::create_dir_all(&tmp);
        std::env::set_var("DSHLAUNCHER_LOG_DIR", &tmp);
        info("单元测试日志写入验证");
        let path = log_path();
        assert!(path.is_file(), "日志文件应已创建：{}", path.display());
        assert!(
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("launcher-") && n.ends_with(".log")),
            "文件名应带日期：{}",
            path.display()
        );
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            content.contains("单元测试日志写入验证"),
            "日志内容应包含写入信息，实际：{content}"
        );
        std::env::remove_var("DSHLAUNCHER_LOG_DIR");
        let _ = std::fs::remove_file(&path);
    }
}
