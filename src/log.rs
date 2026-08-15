//! 极简文件日志：追加写入 ~/.dsh/launcher.log（测试实例带后缀 launcher-<INSTANCE>.log）
//!
//! 守护程序无控制台窗口，故障诊断依赖此日志（FIXLIST P0-2）。
//! 记录点：watchdog 端口状态与拉起、启动流程起止、Job 操作成败、stop 脚本执行。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// 日志文件路径：%USERPROFILE%\.dsh\launcher[-(INSTANCE)].log
/// 调试/测试钩子：DSHLAUNCHER_LOG_DIR 环境变量可覆盖日志目录（如沙箱/CI 环境）。
fn log_path() -> PathBuf {
    let file_name = match std::env::var("DSHLAUNCHER_INSTANCE") {
        Ok(s) if !s.is_empty() => format!("launcher-{s}.log"),
        _ => "launcher.log".to_string(),
    };
    let base = match std::env::var("DSHLAUNCHER_LOG_DIR") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".dsh")
        }
    };
    base.join(file_name)
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

fn write(level: &str, msg: &str) {
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

/// 记录一条 ERROR 日志
pub fn error(msg: &str) {
    write("ERROR", msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_write_creates_file() {
        // 使用临时目录 + DSHLAUNCHER_LOG_DIR 钩子，不依赖用户主目录权限
        let tmp = std::env::temp_dir().join("dsh-launcher-log-test");
        let _ = std::fs::create_dir_all(&tmp);
        std::env::set_var("DSHLAUNCHER_LOG_DIR", &tmp);
        info("单元测试日志写入验证");
        let path = log_path();
        assert!(path.is_file(), "日志文件应已创建：{}", path.display());
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            content.contains("单元测试日志写入验证"),
            "日志内容应包含写入信息，实际：{content}"
        );
        std::env::remove_var("DSHLAUNCHER_LOG_DIR");
        let _ = std::fs::remove_file(&path);
    }
}
