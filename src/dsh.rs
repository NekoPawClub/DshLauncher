//! dsh（DeepSeek Harness）进程控制：启动、停止、页面打开、端口探测

use std::io;
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;
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
$shell = New-Object -ComObject Shell.Application
$prefix = 'file:///' + ($target -replace '\\','/')
$found = $false
foreach ($w in $shell.Windows()) {{
    $loc = $w.LocationURL
    if ($loc -and $loc.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {{
        try {{ $w.Visible = $true }} catch {{}}
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

/// 构造停止 dsh 的 PowerShell 脚本：
/// 先按 Web 端口找到监听进程并连同子进程树一起结束，
/// 再兜底清理命令行指向 @deepseek-ai/dsh 的残留 node 进程。
///
/// 调试开关：环境变量 DSHLAUNCHER_SAFE_TEST=1 时仅按端口清理，
/// 跳过全局 node 匹配，避免在开发机上误杀真实环境里的 dsh 进程。
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

/// 同步停止 dsh：等待清理完成（退出菜单用，确保 Harness 已停止）
pub fn stop_harness() {
    if let Ok(mut child) = run_ps_hidden(&stop_script()) {
        let _ = child.wait();
    }
}

/// 异步发起停止：不等待清理完成即返回（启动流程/重启菜单在动画期间并行清理用）
pub fn stop_harness_async() -> io::Result<Child> {
    run_ps_hidden(&stop_script())
}

/// 后台隐藏窗口启动 dsh：npx @deepseek-ai/dsh web（工作目录为用户主目录）
pub fn start_harness() -> io::Result<()> {
    let home = home_dir();
    let script = format!(
        "Start-Process -FilePath 'cmd.exe' -ArgumentList '/c','npx @deepseek-ai/dsh web' -WorkingDirectory '{home}' -WindowStyle Hidden"
    );
    run_ps_hidden(&script).map(|_| ())
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
