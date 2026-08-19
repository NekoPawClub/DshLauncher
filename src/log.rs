//! 极简文件日志：单文件追加写入 ~/.dsh/launcher.log (测试实例 launcher-<instance>.log)，
//! 保留最近 3 天，过时日志按每行开头的时间标签清理；单文件超过 10 MiB 时
//! 按行截断到 8 MiB 以内，避免常驻 dsh 输出无限增长。
//!
//! 守护程序无控制台窗口，故障诊断依赖此日志。
//! 写入、查询与清理统一由后台日志管理线程串行执行；其它线程只向消息队列投递，
//! 主线程不会因日志文件读改写 (含 3 天清理) 而阻塞。
//! 记录点：watchdog 端口状态与拉起、启动流程起止、Job 操作成败、stop 清理流程、
//! dsh 子进程 (npx/node) 输出、更新检测与更新通知。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// 日志保留天数 (含今天)
const KEEP_DAYS: i64 = 3;
/// 单文件大小上限：追加写后超过该值即按行截断。
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
/// 按大小截断的目标上限：保留文件尾部不超过该字节数。
const MAX_KEEP_BYTES: u64 = 8 * 1024 * 1024;

/// 日志管理线程的命令队列：写日志、查询最近通知版本、刷新等待都由该线程串行处理。
enum LogCommand {
    Write { level: &'static str, msg: String },
    ReadLastNotifiedVersion { reply: SyncSender<Option<String>> },
    Flush { reply: SyncSender<()> },
}

/// 全局日志队列发送端 (首次写日志时启动管理线程)。
static LOG_TX: OnceLock<Sender<LogCommand>> = OnceLock::new();

fn log_tx() -> Sender<LogCommand> {
    LOG_TX
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            thread::Builder::new()
                .name("dsh-launcher-log".to_string())
                .spawn(move || log_loop(rx))
                .expect("无法启动日志管理线程");
            tx
        })
        .clone()
}

/// 日志管理线程：串行执行清理与追加写入；查询在写入之间完成。
/// 同一时刻只有该线程访问日志文件。单条消息处理异常时写内部错误后继续，
/// 避免日志线程退出导致全部日志静默丢失。
fn log_loop(rx: mpsc::Receiver<LogCommand>) {
    let mut last_cleanup_day: Option<(i64, u32, u32)> = None;
    while let Ok(cmd) = rx.recv() {
        let result = catch_unwind(AssertUnwindSafe(|| match cmd {
            LogCommand::Write { level, msg } => {
                cleanup_if_day_changed(&mut last_cleanup_day);
                append_line(level, &msg);
            }
            LogCommand::ReadLastNotifiedVersion { reply } => {
                let _ = reply.send(read_last_logged_notify_version_now());
            }
            LogCommand::Flush { reply } => {
                let _ = reply.send(());
            }
        }));
        if result.is_err() {
            log_internal_error("日志管理线程处理消息异常，继续服务后续消息");
        }
    }
}

/// 日志目录：DSHLAUNCHER_LOG_DIR 覆盖 (沙箱/CI 调试钩子)，否则 %USERPROFILE%/.dsh
pub(crate) fn log_base_dir() -> PathBuf {
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

/// 实例名 (DSHLAUNCHER_INSTANCE)：统一 trim 并只保留字母/数字/连字符/下划线，
/// 日志文件名与单实例互斥体共用同一清洗规则，避免路径穿越与命名不一致。
pub(crate) fn instance_id() -> String {
    std::env::var("DSHLAUNCHER_INSTANCE")
        .map(|s| sanitize_instance_id(&s))
        .unwrap_or_default()
}

/// trim 并只保留字母/数字/连字符/下划线，避免路径穿越与命名不一致
fn sanitize_instance_id(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// 实例后缀 (测试实例隔离日志文件)，含尾部连字符；空实例名无后缀
fn instance_suffix() -> String {
    let id = instance_id();
    if id.is_empty() {
        String::new()
    } else {
        format!("{id}-")
    }
}

/// 日志文件名：正常为 launcher.log；测试实例为 launcher-<instance>.log
fn log_file_name() -> String {
    let id = instance_id();
    if id.is_empty() {
        "launcher.log".to_string()
    } else {
        format!("launcher-{id}.log")
    }
}

/// 当前日志文件路径：{base}/launcher.log (测试实例为 launcher-<instance>.log)
fn log_path() -> PathBuf {
    log_base_dir().join(log_file_name())
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

/// 民用日期 → 自 1970-01-01 起的天数 (Howard Hinnant 算法，正确处理闰年)
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

/// 天数 → 民用日期 (Howard Hinnant 算法)
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

/// 当前时刻归属的日志日 (凌晨 4 点前归前一天)
pub(crate) fn current_log_day() -> (i64, u32, u32) {
    unsafe {
        let mut st: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut st);
        log_day_of(&st)
    }
}

/// 计算 SYSTEMTIME 归属的日志日 (凌晨 4 点为日界)
fn log_day_of(st: &SYSTEMTIME) -> (i64, u32, u32) {
    let days = days_from_civil(st.wYear as i64, st.wMonth as u32, st.wDay as u32);
    let adjusted = if st.wHour < 4 { days - 1 } else { days };
    civil_from_days(adjusted)
}

/// 从日志行开头解析本地时间标签 YYYY-MM-DD HH:MM:SS。
/// 无法解析时返回 None (调用方会保守保留该行，避免误删)。
fn parse_timestamp(line: &str) -> Option<SYSTEMTIME> {
    let b = line.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b' '
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }

    Some(SYSTEMTIME {
        wYear: line.get(0..4)?.parse().ok()?,
        wMonth: line.get(5..7)?.parse().ok()?,
        wDay: line.get(8..10)?.parse().ok()?,
        wHour: line.get(11..13)?.parse().ok()?,
        wMinute: line.get(14..16)?.parse().ok()?,
        wSecond: line.get(17..19)?.parse().ok()?,
        wMilliseconds: 0,
        wDayOfWeek: 0,
    })
}

/// 从“更新通知成功”日志行提取远端版本号与该行归属的日志日。
/// 只识别时间标签后紧跟 [INFO] 的 launcher 日志行，避免混入 dsh 输出或检测行误命中。
fn parse_update_notify_line(line: &str) -> Option<(String, (i64, u32, u32))> {
    let st = parse_timestamp(line)?;
    let message = line.get("YYYY-MM-DD HH:MM:SS".len()..)?;
    let message = message.strip_prefix(" [INFO] ")?;
    let message = message.strip_prefix("更新通知成功：远端 v")?;
    let version = message
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>();
    if version.is_empty() {
        return None;
    }
    Some((version, log_day_of(&st)))
}

/// 读取 launcher.log 中最近一次已通知成功的远端版本号。
/// 通过消息队列交给日志管理线程读取，避免更新检测线程与清理/追加写入并发操作文件。
/// 只统计仍在 3 天保留窗口内的通知行；过期的通知行等同不存在，
/// 下次检查会允许再次提示。
pub(crate) fn last_logged_notify_version() -> Option<String> {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if log_tx()
        .send(LogCommand::ReadLastNotifiedVersion { reply: reply_tx })
        .is_err()
    {
        return None;
    }
    reply_rx
        .recv_timeout(Duration::from_secs(30))
        .ok()
        .flatten()
}

/// 日志管理线程内执行的读取：同一时刻只有本线程访问日志文件。
fn read_last_logged_notify_version_now() -> Option<String> {
    let today = current_log_day();
    let cutoff = days_from_civil(today.0, today.1, today.2) - (KEEP_DAYS - 1);
    let file = File::open(log_path()).ok()?;
    scan_last_logged_notify_version(BufReader::new(file), cutoff)
}

/// 从任意 BufRead 流中扫描 3 天窗口内最后一条通知成功版本记录
/// (从文件读取逻辑拆出，便于测试)
fn scan_last_logged_notify_version<R: BufRead>(mut reader: R, cutoff: i64) -> Option<String> {
    let mut buf = Vec::new();
    let mut last = None;
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        if let Some((version, day)) = parse_update_notify_line(&line) {
            if days_from_civil(day.0, day.1, day.2) >= cutoff {
                last = Some(version);
            }
        }
    }
    last
}

/// 判断一行日志是否应保留：无时间标签的行保留；有时间标签的按日志日 (凌晨 4 点日界)
/// 判断是否早于 cutoff (自 1970-01-01 的天数)。
/// 更新检测行与更新通知行都按普通日志参与 3 天清理，不永久保留。
fn should_keep_log_line(line: &str, cutoff: i64) -> bool {
    match parse_timestamp(line) {
        Some(st) => {
            let (y, m, d) = log_day_of(&st);
            days_from_civil(y, m, d) >= cutoff
        }
        None => true,
    }
}

/// 按行首时间标签清理单文件日志：只保留 cutoff 及之后的日志行。
/// 流式逐行写入临时文件后替换，避免大日志整体读入内存；
/// 调用方是日志管理线程，替换期间没有本程序其它写入者。
fn prune_log_file(path: &Path, cutoff: i64) {
    let Ok(input) = File::open(path) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    let Ok(output) = File::create(&tmp) else {
        return;
    };

    let mut reader = BufReader::new(input);
    let mut writer = BufWriter::new(output);
    let mut buf = Vec::new();
    let mut total = 0u64;
    let mut kept = 0u64;
    let mut failed = false;

    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                let line = String::from_utf8_lossy(&buf);
                if should_keep_log_line(&line, cutoff) {
                    if writer.write_all(&buf).is_err() {
                        failed = true;
                        break;
                    }
                    kept += buf.len() as u64;
                }
            }
            Err(e) => {
                log_internal_error(&format!("读取日志失败 {}: {e}", path.display()));
                failed = true;
                break;
            }
        }
    }

    if failed || writer.flush().is_err() {
        drop(reader);
        drop(writer);
        let _ = std::fs::remove_file(&tmp);
        log_internal_error(&format!(
            "清理日志失败 {} (写入临时文件失败)",
            path.display()
        ));
        return;
    }

    drop(reader);
    drop(writer);

    if kept == total {
        let _ = std::fs::remove_file(&tmp);
        return;
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        log_internal_error(&format!("替换日志失败 {}: {e}", path.display()));
        let _ = std::fs::remove_file(&tmp);
    }
}

/// 从文件头部按行截断：先定位到 skip 字节处，再前进到下一行边界，
/// 把剩余完整行写入临时文件并替换原文件。结果大小不超过原始长度减去 skip。
/// 若 skip 之后直到 EOF 都没有换行，则保留从 skip 开始的尾部 (允许首行不完整)。
fn truncate_log_from_head(path: &Path, skip: u64) -> std::io::Result<()> {
    let mut reader = BufReader::new(File::open(path)?);
    reader.seek(SeekFrom::Start(skip))?;
    let tmp = path.with_extension("tmp");
    let output = File::create(&tmp)?;
    {
        let mut writer = BufWriter::new(output);
        if skip > 0 {
            let mut first = Vec::new();
            let n = reader.read_until(b'\n', &mut first)?;
            if n == 0 || !first.ends_with(b"\n") {
                reader.seek(SeekFrom::Start(skip))?;
            }
        }
        std::io::copy(&mut reader, &mut writer)?;
        writer.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 日志管理线程内执行：单文件超过 MAX_LOG_BYTES 时，把头部按行截掉，
/// 使剩余大小不超过 MAX_KEEP_BYTES。
fn enforce_log_size_limit(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= MAX_LOG_BYTES {
        return;
    }
    let skip = meta.len() - MAX_KEEP_BYTES;
    if let Err(e) = truncate_log_from_head(path, skip) {
        let _ = std::fs::remove_file(path.with_extension("tmp"));
        log_internal_error(&format!("按大小截断日志失败 {}: {e}", path.display()));
    }
}

/// 日志模块自身的错误写入当前日志文件；不经过消息队列，避免在队列/线程异常时递归。
fn log_internal_error(msg: &str) {
    let line = format!(
        "{} [FAIL] {}
",
        timestamp(),
        msg
    );
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    else {
        return;
    };
    let _ = file.write_all(line.as_bytes());
}

/// 删除按天拆分的日志文件 (launcher-YYYY-MM-DD.log 或 launcher-test-YYYY-MM-DD.log)，
/// 仍按文件名中的日期判断是否过期。
fn remove_legacy_daily_logs(base: &Path, cutoff: i64) {
    let prefix = format!("launcher-{}", instance_suffix());
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // prefix 已含尾部连字符 (launcher- 或 launcher-test-)，直接剥离后应为日期
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
        if days_from_civil(y, m, d) < cutoff {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                log_internal_error(&format!("清理过期日志失败 {}: {e}", entry.path().display()));
            }
        }
    }
}

/// 跨凌晨 4 点日志日后执行一次清理：删除按天日志文件并流式裁剪 launcher.log。
/// 只在日志管理线程内调用，清理期间该线程停止消费队列，但主线程与 dsh 读取线程不受阻塞。
fn cleanup_if_day_changed(last_cleanup_day: &mut Option<(i64, u32, u32)>) {
    let today = current_log_day();
    if *last_cleanup_day == Some(today) {
        return;
    }
    *last_cleanup_day = Some(today);

    let today_days = days_from_civil(today.0, today.1, today.2);
    let cutoff = today_days - (KEEP_DAYS - 1); // 今天及之前 2 天保留

    let base = log_base_dir();
    remove_legacy_daily_logs(base.as_path(), cutoff);

    let path = log_path();
    prune_log_file(path.as_path(), cutoff);
}

/// 日志管理线程内执行：追加一行日志。所有调用方都已经把消息放入队列。
fn append_line(level: &str, msg: &str) {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!(
        "{} [{}] {}
",
        timestamp(),
        level,
        msg
    );
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if f.write_all(line.as_bytes()).is_err() {
                log_internal_error(&format!("写入日志失败 {}", path.display()));
            }
        }
        Err(e) => {
            log_internal_error(&format!("打开日志文件失败 {}: {e}", path.display()));
        }
    }
    enforce_log_size_limit(path.as_path());
}

fn write(level: &'static str, msg: &str) {
    // 主线程与各后台线程都只投递消息；真正的磁盘写入/清理在日志管理线程完成。
    let _ = log_tx().send(LogCommand::Write {
        level,
        msg: msg.to_string(),
    });
}

/// 等待日志管理线程处理完已投递的全部消息。
/// 供测试与正常退出收尾使用 (进程被强杀时仍允许丢失最后少量日志)。
pub fn flush() {
    let Some(tx) = LOG_TX.get() else {
        return;
    };
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if tx.send(LogCommand::Flush { reply: reply_tx }).is_ok() {
        let _ = reply_rx.recv_timeout(Duration::from_secs(5));
    }
}

/// 记录一条 INFO 日志
pub fn info(msg: &str) {
    write("INFO", msg);
}

/// 记录一行 dsh 子进程输出 (stdout/stderr 合并写入 launcher.log)
pub(crate) fn dsh_output(line: &str) {
    write("DSH", line);
}

/// 记录一条 WARN 日志
pub fn warn(msg: &str) {
    write("WARN", msg);
}

/// 记录一条 FAIL 日志 (4 字符标签，与 INFO/WARN 对齐)
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

        // 世纪闰年：2000 年 3 月 1 日 03:00 → 2 月 29 日 (2000 是闰年)
        st.wYear = 2000;
        st.wMonth = 3;
        st.wDay = 1;
        st.wHour = 3;
        assert_eq!(log_day_of(&st), (2000, 2, 29), "世纪闰年 2000");

        // 非世纪闰年：1900 年 3 月 1 日 03:00 → 2 月 28 日 (1900 不是闰年)
        st.wYear = 1900;
        st.wMonth = 3;
        st.wDay = 1;
        st.wHour = 3;
        assert_eq!(log_day_of(&st), (1900, 2, 28), "世纪非闰年 1900");
    }

    /// 日志写入与文件创建 (临时目录 + LOG_DIR 钩子，不依赖用户主目录权限)
    #[test]
    fn log_write_creates_file() {
        // 注意：set_var 修改进程环境变量，仅本测试使用且无其他测试并发读取该变量
        let tmp = std::env::temp_dir().join("dsh-launcher-log-test");
        let _ = std::fs::create_dir_all(&tmp);
        std::env::set_var("DSHLAUNCHER_LOG_DIR", &tmp);
        std::env::set_var("DSHLAUNCHER_INSTANCE", "");
        info("单元测试日志写入验证");
        flush();
        let path = log_path();
        assert!(path.is_file(), "日志文件应已创建：{}", path.display());
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("launcher.log"),
            "应写入单个日志文件 launcher.log，实际：{}",
            path.display()
        );
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            content.contains("单元测试日志写入验证"),
            "日志内容应包含写入信息，实际：{content}"
        );

        // 验证异步日志队列的读取查询路径：写 -> flush -> 管理线程内流式读取
        info("更新通知成功：远端 v26.08.18.01");
        flush();
        assert_eq!(
            last_logged_notify_version().as_deref(),
            Some("26.08.18.01"),
            "日志管理线程应能读取到刚写入的通知成功版本"
        );

        std::env::remove_var("DSHLAUNCHER_INSTANCE");
        std::env::remove_var("DSHLAUNCHER_LOG_DIR");
        let _ = std::fs::remove_file(&path);
    }

    /// 按行首时间标签清理过时日志
    #[test]
    fn prune_log_file_removes_outdated_lines() {
        let tmp = std::env::temp_dir().join("dsh-launcher-prune-test");
        let _ = std::fs::create_dir_all(&tmp);
        let path = tmp.join("launcher-prune-test.log");
        // 保留 2026-08-14 及之后 (含 08-15 凌晨 3 点，按 4 点日界仍归 08-14)
        let cutoff = days_from_civil(2026, 8, 14);
        let content = concat!(
            "2026-08-13 23:59:59 [INFO] old\n",
            "2020-01-01 00:00:00 [INFO] 更新检测成功：远端 v20.01.01.01 (本地 v20.01.01.00)\n",
            "2020-01-01 00:00:01 [INFO] 更新通知成功：远端 v20.01.01.01\n",
            "2026-08-14 04:00:00 [INFO] keep-day-14\n",
            "no-timestamp [INFO] keep\n",
            "2026-08-15 03:59:59 [INFO] keep-day-15\n"
        );
        std::fs::write(&path, content.as_bytes()).unwrap();

        prune_log_file(path.as_path(), cutoff);

        let pruned = std::fs::read_to_string(&path).unwrap();
        assert!(!pruned.contains("old"), "过时行应被删除，实际：{pruned}");
        assert!(
            pruned.contains("keep-day-14"),
            "边界行应保留，实际：{pruned}"
        );
        assert!(
            pruned.contains("no-timestamp"),
            "无时间标签行应保留，实际：{pruned}"
        );
        assert!(
            pruned.contains("keep-day-15"),
            "按 4 点日界应保留，实际：{pruned}"
        );
        assert!(
            !pruned.contains("更新检测成功：远端 v20.01.01.01"),
            "过期更新检测行应按普通日志删除，实际：{pruned}"
        );
        assert!(
            !pruned.contains("更新通知成功：远端 v20.01.01.01"),
            "过期通知成功行应按普通日志删除，实际：{pruned}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 时间标签解析
    #[test]
    fn parse_timestamp_parses_log_prefix() {
        let st = parse_timestamp("2026-08-15 03:59:59 [INFO] x").unwrap();
        assert_eq!(
            (st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond),
            (2026, 8, 15, 3, 59, 59)
        );
        assert!(parse_timestamp("no timestamp").is_none());
    }

    /// 通知成功版本从“更新通知成功”日志行提取；检测行与 dsh 输出不会误命中
    #[test]
    fn parse_update_notify_line_reads_remote_version() {
        let (version, day) =
            parse_update_notify_line("2026-08-18 03:59:59 [INFO] 更新通知成功：远端 v26.08.18.01")
                .expect("应解析成功");
        assert_eq!(version, "26.08.18.01");
        assert_eq!(day, (2026, 8, 17), "凌晨 4 点前应归前一天");
        assert!(parse_update_notify_line(
            "2026-08-18 10:00:00 [DSH] [INFO] 更新通知成功：远端 v26.08.18.01"
        )
        .is_none());
        assert!(parse_update_notify_line(
            "2026-08-18 10:00:00 [INFO] 更新检测成功：远端 v26.08.18.01 (本地 v26.08.18.00)"
        )
        .is_none());
    }

    /// 只读取 3 天窗口内的通知成功版本记录；过期记录视为不存在
    #[test]
    fn scan_last_logged_notify_version_respects_retention() {
        let cutoff = days_from_civil(2026, 8, 14);
        let content = concat!(
            "2026-08-13 23:59:59 [INFO] 更新通知成功：远端 v26.08.13.01\n",
            "2026-08-14 04:00:00 [INFO] 更新通知成功：远端 v26.08.14.01\n",
            "2026-08-15 03:59:59 [INFO] 更新通知成功：远端 v26.08.15.01\n",
        );
        assert_eq!(
            scan_last_logged_notify_version(std::io::Cursor::new(content.as_bytes()), cutoff)
                .as_deref(),
            Some("26.08.15.01")
        );

        let expired = "2026-08-13 23:59:59 [INFO] 更新通知成功：远端 v26.08.13.01\n";
        assert_eq!(
            scan_last_logged_notify_version(std::io::Cursor::new(expired.as_bytes()), cutoff),
            None
        );
    }

    /// 超过大小上限后按行截断到目标字节数以内，并保留最新完整行
    #[test]
    fn truncate_log_from_head_keeps_tail_within_target() {
        let tmp = std::env::temp_dir().join("dsh-launcher-size-test");
        let _ = std::fs::create_dir_all(&tmp);
        let path = tmp.join("launcher-size-test.log");
        let content = "line-01\nline-02\nline-03\nline-04\nline-05\n";
        std::fs::write(&path, content.as_bytes()).unwrap();

        truncate_log_from_head(path.as_path(), 16).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() <= content.len() - 16);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.ends_with("line-05\n"));
        assert!(text.starts_with("line-04\n") || text.starts_with("line-03\n"));
        let _ = std::fs::remove_file(&path);
    }

    /// 大小截断函数在无换行的异常大行上也保持有界输出
    #[test]
    fn truncate_log_from_head_handles_missing_newline() {
        let tmp = std::env::temp_dir().join("dsh-launcher-size-test");
        let _ = std::fs::create_dir_all(&tmp);
        let path = tmp.join("launcher-size-no-newline.log");
        std::fs::write(&path, b"0123456789abcdef").unwrap();

        truncate_log_from_head(path.as_path(), 6).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() <= 10);
        assert_eq!(bytes, b"6789abcdef");
        let _ = std::fs::remove_file(&path);
    }

    /// 实例名清洗：trim + 只保留安全字符，避免日志文件路径穿越
    #[test]
    fn instance_id_sanitizes_env_value() {
        assert_eq!(sanitize_instance_id("  test_01  "), "test_01");
        assert_eq!(sanitize_instance_id("../evil\\path"), "evilpath");
        assert_eq!(sanitize_instance_id("  "), "");
    }
}
