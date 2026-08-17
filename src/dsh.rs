//! dsh（DeepSeek Harness）进程控制：启动、停止、页面打开、端口探测

use std::io;
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, CREATE_NO_WINDOW, IO_COUNTERS};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// dsh Web 默认端口（dsh web 默认监听 3080）
pub const DEFAULT_PORT: u16 = 3080;

/// 从环境变量字符串解析端口（无效/0/超范围回退默认值）
fn port_from_env(value: Option<&str>) -> u16 {
    value
        .and_then(|v| v.trim().parse::<u16>().ok())
        .filter(|p| *p > 0)
        .unwrap_or(DEFAULT_PORT)
}

/// 获取 dsh Web 端口：优先读取环境变量 DSHLAUNCHER_PORT，否则使用默认值
pub fn web_port() -> u16 {
    port_from_env(std::env::var("DSHLAUNCHER_PORT").ok().as_deref())
}

/// dsh 操作页面地址
pub fn web_url() -> String {
    format!("http://127.0.0.1:{}", web_port())
}

/// 用户主目录（~）
fn home_dir() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string())
}

/// 把字符串转为带终止符的 UTF-16（供 Win32 API 使用）
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 转义 PowerShell 单引号字符串内的单引号（单引号双写），
/// 防止路径含单引号（如用户名 O'Reilly）破坏脚本字符串
fn ps_quote_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// 检查指定端口当前是否可连接（运行中）
fn port_ready_at(port: u16) -> bool {
    use std::net::SocketAddr;
    // connect_timeout：避免回环被防火墙 DROP 时探测挂起拖慢 watchdog
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// 检查 dsh 端口当前是否可连接（运行中）
pub fn port_ready() -> bool {
    port_ready_at(web_port())
}

/// 检查指定端口是否被占用（不可连接但 bind 失败 = 有残留进程占着端口）
fn port_occupied_at(port: u16) -> bool {
    !port_ready_at(port) && std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
}

/// 检查 dsh 端口是否被占用（不可连接但 bind 失败 = 有残留进程占着端口）。
/// bind 探测毫秒级、无 PowerShell 开销。
pub fn port_occupied() -> bool {
    port_occupied_at(web_port())
}

/// HANDLE 的 Send+Sync 包装（静态 Mutex 变量要求裸指针可跨线程共享）
struct JobHandle(HANDLE);
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

/// 全局 Job 对象：管理 DshLauncher 启动的 dsh 进程树。
/// 设置 KILL_ON_JOB_CLOSE：DshLauncher 进程退出（含崩溃/被强杀）时，
/// Windows 自动终止 Job 内所有进程 —— 从设计上保证不会遗留孤儿 dsh，
/// 因此常态启动/退出无需"清理残留"流程。
/// 创建失败不缓存：下次调用重新尝试。
fn job_handle() -> Option<HANDLE> {
    static JOB: Mutex<Option<JobHandle>> = Mutex::new(None);
    let mut guard = JOB.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(jh) = guard.as_ref() {
        return Some(jh.0);
    }
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            crate::log::error("CreateJobObjectW 失败（Job 不可用，dsh 将无法被自动回收）");
            return None;
        }
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                PerProcessUserTimeLimit: 0,
                PerJobUserTimeLimit: 0,
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                MinimumWorkingSetSize: 0,
                MaximumWorkingSetSize: 0,
                ActiveProcessLimit: 0,
                Affinity: 0,
                PriorityClass: 0,
                SchedulingClass: 0,
            },
            IoInfo: IO_COUNTERS {
                ReadOperationCount: 0,
                WriteOperationCount: 0,
                OtherOperationCount: 0,
                ReadTransferCount: 0,
                WriteTransferCount: 0,
                OtherTransferCount: 0,
            },
            ProcessMemoryLimit: 0,
            JobMemoryLimit: 0,
            PeakProcessMemoryUsed: 0,
            PeakJobMemoryUsed: 0,
        };
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            // KILL_ON_JOB_CLOSE 未生效时该 Job 无回收能力：不缓存，释放后下次重试
            crate::log::error(
                "SetInformationJobObject 失败（KILL_ON_JOB_CLOSE 未生效，dsh 退出时可能残留）",
            );
            CloseHandle(job);
            return None;
        }
        *guard = Some(JobHandle(job));
        Some(job)
    }
}

/// 用系统默认方式（ShellExecuteW）打开目标（URL / 文件 / 目录），不产生任何控制台窗口
fn shell_execute_open(target: &str) -> io::Result<()> {
    let target_wide = to_wide(target);
    unsafe {
        let result = ShellExecuteW(
            std::ptr::null_mut(), // 无父窗口
            to_wide("open").as_ptr(),
            target_wide.as_ptr(), // 目标：URL / 文件 / 目录
            std::ptr::null(),     // 无参数
            std::ptr::null(),     // 无工作目录
            SW_SHOWNORMAL,
        );
        // 返回值大于 32 表示成功
        if result as isize > 32 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// 用系统默认方式（ShellExecuteW）打开 dsh 操作页面，不产生任何控制台窗口
pub fn open_page() -> io::Result<()> {
    shell_execute_open(&web_url())
}

/// 用资源管理器打开 dsh 配置目录（~/.dsh）。
/// 先确保目录存在，再通过 ShellExecuteW 直接让系统资源管理器打开；
/// 不再依赖隐藏 PowerShell 进程里的 Shell.Application COM 枚举（该方案在部分环境不弹窗）。
pub fn open_config_dir() -> io::Result<()> {
    let dir = Path::new(&home_dir()).join(".dsh");
    // 目录不存在时先创建，避免 ShellExecuteW 打开不存在的路径而失败
    std::fs::create_dir_all(&dir)?;
    let dir_str = dir.to_string_lossy().into_owned();
    shell_execute_open(&dir_str)
}

/// 构造停止 dsh 的 PowerShell 脚本（仅作为兜底：处理端口被外部进程占用、
/// 非 Job 管理的残留；正常路径由 TerminateJobObject 秒杀，不走此脚本）：
/// 按 Web 端口找监听进程并连同子进程树结束。
fn stop_script_with(port: u16) -> String {
    let port_part = r#"
# 1) 结束监听 dsh 端口的进程及其子进程树
netstat -ano | Select-String ("TCP\s+\S*:" + $port + "\s") | Select-String "LISTENING" | ForEach-Object {{
    $parts = ($_.ToString().Trim() -split '\s+')
    $procId = 0
    if ([int]::TryParse($parts[$parts.Count - 1], [ref]$procId)) {{
        taskkill /PID $procId /T /F 2>$null | Out-Null
    }}
}}
"#
    .to_string();
    format!(
        "$ErrorActionPreference = 'SilentlyContinue'\n$port = {}\n{}",
        port, port_part
    )
}

/// 构造停止 dsh 的 PowerShell 脚本（使用当前端口）
fn stop_script() -> String {
    stop_script_with(web_port())
}

/// 停止 dsh：优先 TerminateJobObject 秒杀整个进程树（毫秒级、无 PowerShell）；
/// 若端口仍被外部进程占用（非 Job 管理，如用户手动启动的 dsh），
/// 再兜底按端口清理（罕见场景）。
pub fn stop_harness() {
    // 1) 秒杀 Job 内进程树
    if let Some(job) = job_handle() {
        unsafe {
            if TerminateJobObject(job, 1) == 0 {
                crate::log::warn("TerminateJobObject 失败（Job 秒杀未生效）");
            }
        }
    }
    // 2) 等终止生效，检查端口是否仍被占
    thread::sleep(Duration::from_millis(300));
    if port_ready() || port_occupied() {
        crate::log::info("端口仍被占用，执行兜底清理脚本");
        if let Ok(mut child) = run_ps_hidden(&stop_script()) {
            // 兜底脚本最多等 5 秒，超时强杀，避免退出流程卡死
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    crate::log::warn("兜底清理脚本超时，已强制终止");
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// 后台隐藏窗口启动 dsh：npx @deepseek-ai/dsh web（工作目录为用户主目录）。
/// 端口覆盖时透传 --port；-y 避免 npx 首次安装的交互确认。
/// 启动后立即把进程树挂入全局 Job（KILL_ON_JOB_CLOSE）。
/// quitting：退出请求标志，spawn 后立即检查，避免退出竞态导致进程树漏挂 Job。
pub fn start_harness(quitting: &AtomicBool) -> io::Result<()> {
    let home = home_dir();
    let port = web_port();
    let web_cmd = if port == DEFAULT_PORT {
        "npx -y @deepseek-ai/dsh web".to_string()
    } else {
        format!("npx -y @deepseek-ai/dsh web --port {port}")
    };
    // 工作目录统一正斜杠（Windows 兼容，cmd/CreateProcess 均接受）
    let home_esc = ps_quote_escape(&home.replace('\\', "/"));
    let script = format!(
        "Start-Process -FilePath 'cmd.exe' -ArgumentList '/c','{web_cmd}' -WorkingDirectory '{home_esc}' -WindowStyle Hidden"
    );
    let mut child = run_ps_hidden(&script)?;
    // 退出竞态加固：spawn 后若收到退出请求，立即终止刚启动的进程，
    // 避免 Job 挂接前退出导致 main 尾部清理漏杀此进程树
    if quitting.load(Ordering::SeqCst) {
        let _ = child.kill();
        return Err(io::Error::new(io::ErrorKind::Interrupted, "收到退出请求"));
    }
    // 挂入 Job：DshLauncher 退出/崩溃时自动终止 dsh，不留孤儿。
    // Job 不可用（创建或配置失败）时拒绝启动：dsh 无法被回收 = 孤儿，不允许放行
    match job_handle() {
        Some(job) => {
            use std::os::windows::io::AsRawHandle;
            unsafe {
                // 挂接失败：dsh 将成孤儿，视为启动失败并终止刚启动的进程
                if AssignProcessToJobObject(job, child.as_raw_handle()) == 0 {
                    let err = io::Error::last_os_error();
                    crate::log::error(&format!(
                        "AssignProcessToJobObject 失败（dsh 将无法被 Job 回收）：{err}"
                    ));
                    let _ = child.kill();
                    return Err(err);
                }
            }
        }
        None => {
            let _ = child.kill();
            return Err(io::Error::other(
                "Job 对象不可用（dsh 将无法被自动回收），拒绝启动",
            ));
        }
    }
    crate::log::info(&format!("已启动 dsh（{web_cmd}）并挂入 Job"));
    Ok(())
}

/// 把字符串编码为 UTF-16LE 字节序列（-EncodedCommand 用）
fn utf16le_bytes(s: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(s.len() * 2);
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// 将 PowerShell 脚本编码为 UTF-16LE + Base64（-EncodedCommand 免转义）
fn ps_encoded(script: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(utf16le_bytes(script))
}

/// 以隐藏窗口（CREATE_NO_WINDOW）方式启动 PowerShell 进程执行脚本
fn run_ps_hidden(script: &str) -> io::Result<Child> {
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-EncodedCommand",
            &ps_encoded(script),
        ])
        .creation_flags(CREATE_NO_WINDOW) // 不弹出任何控制台窗口
        .spawn()
}

/// 释放单实例互斥体（退出流程提前调用，让新实例在旧进程退出前即可启动）
pub fn release_single_instance(handle: HANDLE) {
    unsafe {
        CloseHandle(handle);
    }
}

/// 创建命名互斥体实现单实例；返回 None 表示已有实例在运行。
/// 环境变量 DSHLAUNCHER_INSTANCE 可附加互斥体后缀（测试实例隔离用，
/// 让测试实例与用户实例互不干扰）。
pub fn single_instance_guard() -> Option<HANDLE> {
    let suffix = std::env::var("DSHLAUNCHER_INSTANCE").unwrap_or_default();
    let name_str = format!("Local\\DshLauncher.SingleInstance{}", suffix);
    let name = to_wide(&name_str);
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            return None;
        }
        Some(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_from_env_parses_valid() {
        assert_eq!(port_from_env(Some("39999")), 39999);
        assert_eq!(port_from_env(Some(" 8080 ")), 8080);
    }

    #[test]
    fn port_from_env_rejects_invalid() {
        assert_eq!(port_from_env(Some("abc")), DEFAULT_PORT);
        assert_eq!(port_from_env(Some("0")), DEFAULT_PORT);
        assert_eq!(port_from_env(Some("70000")), DEFAULT_PORT);
        assert_eq!(port_from_env(Some("")), DEFAULT_PORT);
        assert_eq!(port_from_env(None), DEFAULT_PORT);
    }

    #[test]
    fn to_wide_encodes_and_terminates() {
        assert_eq!(to_wide("ab中"), vec![0x61, 0x62, 0x4e2d, 0]);
    }

    #[test]
    fn ps_quote_escape_handles_quotes() {
        assert_eq!(ps_quote_escape("plain"), "plain");
        assert_eq!(ps_quote_escape("O'Reilly"), "O''Reilly");
        assert_eq!(ps_quote_escape("a'b'c"), "a''b''c");
    }

    #[test]
    fn ps_encoded_roundtrip() {
        use base64::Engine;
        let s = "中文 '引号' \\反斜杠\n多行";
        let enc = ps_encoded(s);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(enc)
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), s);
    }

    #[test]
    fn stop_script_keeps_regex_escapes() {
        let s = stop_script_with(3080);
        assert!(s.contains(r"TCP\s+\S*:"), "端口正则转义必须保留，实际：{s}");
        assert!(s.contains("$port = 3080"));
    }

    #[test]
    fn stop_script_uses_given_port() {
        let s = stop_script_with(39999);
        assert!(s.contains("$port = 39999"));
    }

    #[test]
    fn port_probe_semantics() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_ready_at(port), "监听中应就绪");
        assert!(!port_occupied_at(port), "监听中不算被占");
        drop(listener);
        assert!(!port_ready_at(port), "释放后应未就绪");
        assert!(!port_occupied_at(port), "释放后可 bind，不算被占");
    }
}
