//! dsh（DeepSeek Harness）进程控制：启动、停止、页面打开、端口探测

use std::io;
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, IO_COUNTERS};
use windows_sys::Win32::UI::Shell::ShellExecuteW;

/// dsh Web 默认端口（dsh web 默认监听 3080）
pub const DEFAULT_PORT: u16 = 3080;

/// 获取 dsh Web 端口：优先读取环境变量 DSHLAUNCHER_PORT，否则使用默认值
pub fn web_port() -> u16 {
    std::env::var("DSHLAUNCHER_PORT")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .filter(|p| *p > 0)
        .unwrap_or(DEFAULT_PORT)
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

/// 检查 dsh 端口当前是否可连接（运行中）
pub fn port_ready() -> bool {
    TcpStream::connect(("127.0.0.1", web_port())).is_ok()
}

/// 检查 dsh 端口是否被占用（不可连接但 bind 失败 = 有残留进程占着端口）。
/// bind 探测毫秒级、无 PowerShell 开销。
pub fn port_occupied() -> bool {
    !port_ready() && std::net::TcpListener::bind(("127.0.0.1", web_port())).is_err()
}

/// HANDLE 的 Send+Sync 包装（OnceLock 静态变量要求裸指针可跨线程共享）
struct JobHandle(HANDLE);
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

/// 全局 Job 对象：管理 DshLauncher 启动的 dsh 进程树。
/// 设置 KILL_ON_JOB_CLOSE：DshLauncher 进程退出（含崩溃/被强杀）时，
/// Windows 自动终止 Job 内所有进程 —— 从设计上保证不会遗留孤儿 dsh，
/// 因此常态启动/退出无需"清理残留"流程。
fn job_handle() -> Option<HANDLE> {
    static JOB: OnceLock<JobHandle> = OnceLock::new();
    let handle = JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return JobHandle(job);
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
        let _ = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        JobHandle(job)
    })
    .0;
    (!handle.is_null()).then_some(handle)
}

/// 用系统默认方式（ShellExecuteW）打开 dsh 操作页面，不产生任何控制台窗口
pub fn open_page() -> io::Result<()> {
    let url = web_url();
    let url_wide = to_wide(&url);
    unsafe {
        let result = ShellExecuteW(
            std::ptr::null_mut(), // 无父窗口
            to_wide("open").as_ptr(),
            url_wide.as_ptr(), // 目标：URL
            std::ptr::null(),  // 无参数
            std::ptr::null(),  // 无工作目录
            1,                 // SW_SHOWNORMAL
        );
        // 返回值大于 32 表示成功
        if result as isize > 32 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// 用资源管理器打开 dsh 配置目录（~/.dsh）：
/// 若该目录已在某个资源管理器窗口中打开，则激活既有窗口而非新建（参考 VS Code 行为）；
/// 未找到时才新建窗口。
pub fn open_config_dir() -> io::Result<()> {
    let dir = Path::new(&home_dir()).join(".dsh");
    let dir_str = dir.to_string_lossy().to_string();
    let script = format!(
        r#"
$ErrorActionPreference = 'SilentlyContinue'
$target = '{dir}'
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32Activate {{
    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hWnd);
}}
"@
$shell = New-Object -ComObject Shell.Application
$prefix = 'file:///' + ($target -replace '\\','/')
$found = $false
foreach ($w in $shell.Windows()) {{
    $loc = $w.LocationURL
    if ($loc -and $loc.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {{
        try {{
            # 激活既有窗口到前台（VS Code 行为）：恢复最小化 + 置前
            $hwnd = [IntPtr]$w.HWND
            [Win32Activate]::ShowWindow($hwnd, 9) | Out-Null
            [Win32Activate]::SetForegroundWindow($hwnd) | Out-Null
        }} catch {{}}
        $found = $true
        break
    }}
}}
if (-not $found) {{ $null = $shell.Open($target) }}
"#,
        dir = dir_str
    );
    run_ps_hidden(&script).map(|_| ())
}

/// 构造停止 dsh 的 PowerShell 脚本（仅作为兜底：处理端口被外部进程占用、
/// 非 Job 管理的残留；正常路径由 TerminateJobObject 秒杀，不走此脚本）：
/// 按 Web 端口找监听进程并连同子进程树结束，再兜底清理 node 残留。
/// 调试开关：DSHLAUNCHER_SAFE_TEST=1 时仅按端口清理，跳过 node 匹配。
fn stop_script() -> String {
    let safe_test = std::env::var("DSHLAUNCHER_SAFE_TEST").as_deref() == Ok("1");
    let port = web_port();

    let port_part = format!(
        r#"
# 1) 结束监听 dsh 端口的进程及其子进程树
netstat -ano | Select-String ("TCP\s+\S*:" + $port + "\s") | Select-String "LISTENING" | ForEach-Object {{
    $parts = ($_.ToString().Trim() -split '\s+')
    $procId = 0
    if ([int]::TryParse($parts[$parts.Count - 1], [ref]$procId)) {{
        taskkill /PID $procId /T /F 2>$null | Out-Null
    }}
}}
"#
    );
    let fallback_part = r#"
# 2) 兜底：清理命令行指向 dsh 包的残留 node 进程
Get-CimInstance Win32_Process -Filter "Name='node.exe'" | Where-Object { $_.CommandLine -match 'deepseek-ai[\\/]dsh' } | ForEach-Object {
    taskkill /PID $_.ProcessId /T /F 2>$null | Out-Null
}
"#;

    if safe_test {
        format!(
            "$ErrorActionPreference = 'SilentlyContinue'
$port = {}
{}",
            port, port_part
        )
    } else {
        format!(
            "$ErrorActionPreference = 'SilentlyContinue'
$port = {}
{}{}",
            port, port_part, fallback_part
        )
    }
}

/// 停止 dsh：优先 TerminateJobObject 秒杀整个进程树（毫秒级、无 PowerShell）；
/// 若端口仍被外部进程占用（非 Job 管理，如用户手动启动的 dsh），
/// 再兜底按端口清理（罕见场景）。
pub fn stop_harness() {
    // 1) 秒杀 Job 内进程树
    if let Some(job) = job_handle() {
        unsafe {
            let _ = TerminateJobObject(job, 1);
        }
    }
    // 2) 等终止生效，检查端口是否仍被占
    thread::sleep(Duration::from_millis(300));
    if port_ready() || port_occupied() {
        if let Ok(mut child) = run_ps_hidden(&stop_script()) {
            let _ = child.wait();
        }
    }
}

/// 后台隐藏窗口启动 dsh：npx @deepseek-ai/dsh web（工作目录为用户主目录）。
/// 启动后立即把进程树挂入全局 Job（KILL_ON_JOB_CLOSE）。
pub fn start_harness() -> io::Result<()> {
    let home = home_dir();
    let script = format!(
        "Start-Process -FilePath 'cmd.exe' -ArgumentList '/c','npx @deepseek-ai/dsh web' -WorkingDirectory '{home}' -WindowStyle Hidden"
    );
    let child = run_ps_hidden(&script)?;
    // 挂入 Job：DshLauncher 退出/崩溃时自动终止 dsh，不留孤儿
    if let Some(job) = job_handle() {
        use std::os::windows::io::AsRawHandle;
        unsafe {
            let _ = AssignProcessToJobObject(job, child.as_raw_handle());
        }
    }
    Ok(())
}

/// 将 PowerShell 脚本编码为 UTF-16LE + Base64（-EncodedCommand 免转义）
fn ps_encoded(script: &str) -> String {
    use base64::Engine;
    let mut bytes: Vec<u8> = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
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
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW：不弹出任何控制台窗口
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
