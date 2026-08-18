//! dsh (DeepSeek Harness) 进程控制：启动、停止、页面打开、端口探测

use std::io::{self, Read};
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS,
    HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, MIB_TCP_STATE_LISTEN, TCP_TABLE_OWNER_PID_ALL,
};
use windows_sys::Win32::Networking::WinSock::AF_INET;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, TerminateProcess, CREATE_NO_WINDOW, IO_COUNTERS, PROCESS_TERMINATE,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// dsh Web 默认端口 (dsh web 默认监听 3080)
pub const DEFAULT_PORT: u16 = 3080;

/// 从环境变量字符串解析端口 (无效/0/超范围回退默认值)
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

/// 用户主目录 (~)
fn home_dir() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string())
}

/// 把字符串转为带终止符的 UTF-16 (供 Win32 API 使用)
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 检查指定端口当前是否可连接 (运行中)
fn port_ready_at(port: u16) -> bool {
    use std::net::SocketAddr;
    // connect_timeout：避免回环被防火墙 DROP 时探测挂起拖慢 watchdog
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// 检查 dsh 端口当前是否可连接 (运行中)
pub fn port_ready() -> bool {
    port_ready_at(web_port())
}

/// 检查指定端口是否被占用 (不可连接但 bind 失败 = 有残留进程占着端口)
fn port_occupied_at(port: u16) -> bool {
    !port_ready_at(port) && std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
}

/// 检查 dsh 端口是否被占用 (不可连接但 bind 失败 = 有残留进程占着端口)。
/// bind 探测毫秒级、无 PowerShell 开销。
pub fn port_occupied() -> bool {
    port_occupied_at(web_port())
}

/// HANDLE 的 Send+Sync 包装 (静态 Mutex 变量要求裸指针可跨线程共享)
struct JobHandle(HANDLE);
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

/// 全局 Job 对象：管理 DshLauncher 启动的 dsh 进程树。
/// 设置 KILL_ON_JOB_CLOSE：DshLauncher 进程退出 (含崩溃/被强杀) 时，
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
            crate::log::error("CreateJobObjectW 失败 (Job 不可用，dsh 将无法被自动回收)");
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
                "SetInformationJobObject 失败 (KILL_ON_JOB_CLOSE 未生效，dsh 退出时可能残留)",
            );
            CloseHandle(job);
            return None;
        }
        *guard = Some(JobHandle(job));
        Some(job)
    }
}

/// 用系统默认方式 (ShellExecuteW) 打开目标 (URL / 文件 / 目录)，不产生任何控制台窗口
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

/// 用系统默认方式 (ShellExecuteW) 打开 dsh 操作页面，不产生任何控制台窗口
pub fn open_page() -> io::Result<()> {
    shell_execute_open(&web_url())
}

/// 用资源管理器打开 dsh 配置目录 (~/.dsh)。
/// 先确保目录存在，再通过 ShellExecuteW 直接让系统资源管理器打开；
/// 不再依赖隐藏 PowerShell 进程里的 Shell.Application COM 枚举 (该方案在部分环境不弹窗)。
pub fn open_config_dir() -> io::Result<()> {
    let dir = Path::new(&home_dir()).join(".dsh");
    // 目录不存在时先创建，避免 ShellExecuteW 打开不存在的路径而失败
    std::fs::create_dir_all(&dir)?;
    let dir_str = dir.to_string_lossy().into_owned();
    shell_execute_open(&dir_str)
}

/// 查询占用指定 IPv4 端口的监听进程 PID (GetExtendedTcpTable，纯 Win32)。
fn listener_pids(port: u16) -> Vec<u32> {
    unsafe {
        let mut size: u32 = 0;
        let ret = GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
        if ret != ERROR_INSUFFICIENT_BUFFER || size == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; size as usize];
        let ret = GetExtendedTcpTable(
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
        if ret != ERROR_SUCCESS {
            return Vec::new();
        }

        let table = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
        let rows = std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        rows.iter()
            .filter(|row| row.dwState == MIB_TCP_STATE_LISTEN as u32)
            .filter(|row| u16::from_be((row.dwLocalPort & 0xffff) as u16) == port)
            .map(|row| row.dwOwningPid)
            .collect()
    }
}

/// 枚举当前进程快照 (pid, parent_pid)，用于兜底清理外部进程树。
fn process_snapshot() -> Vec<(u32, u32)> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                out.push((entry.th32ProcessID, entry.th32ParentProcessID));
                entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        out
    }
}

/// 用 TerminateProcess 结束单个进程；跳过本进程。
fn terminate_pid(pid: u32) -> bool {
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let ok = TerminateProcess(handle, 1) != 0;
        CloseHandle(handle);
        ok
    }
}

/// 按进程快照递归结束整棵进程树 (先子后父，循环引用由 killed 列表阻断)。
fn kill_tree_from(pid: u32, processes: &[(u32, u32)], killed: &mut Vec<u32>) {
    if pid == 0 || killed.contains(&pid) {
        return;
    }
    for &(child, parent) in processes {
        if parent == pid && !killed.contains(&child) {
            kill_tree_from(child, processes, killed);
        }
    }
    let _ = terminate_pid(pid);
    killed.push(pid);
}

/// 清理一组根进程及其子进程树 (纯 Win32；外部进程兜底路径)。
fn kill_process_tree(roots: &[u32]) {
    let processes = process_snapshot();
    let mut killed = Vec::new();
    for &root in roots {
        kill_tree_from(root, &processes, &mut killed);
    }
}

/// 停止 dsh：优先 TerminateJobObject 秒杀整个进程树 (毫秒级、无外部进程)；
/// 若端口仍被外部进程占用 (非 Job 管理，如用户手动启动的 dsh)，
/// 再用 GetExtendedTcpTable + Toolhelp 快照按进程树兜底清理 (纯 Win32)。
pub fn stop_harness() {
    // 1) 秒杀 Job 内进程树
    if let Some(job) = job_handle() {
        unsafe {
            if TerminateJobObject(job, 1) == 0 {
                crate::log::warn("TerminateJobObject 失败 (Job 秒杀未生效)");
            }
        }
    }
    // 2) 等终止生效，检查端口是否仍被占
    thread::sleep(Duration::from_millis(300));
    if port_ready() || port_occupied() {
        crate::log::info("端口仍被占用，执行纯 Win32 兜底清理");
        let pids = listener_pids(web_port());
        if pids.is_empty() {
            crate::log::warn("未找到占用端口的监听进程，放弃兜底清理");
        } else {
            crate::log::info(&format!("终止占用端口的进程树，根 PID：{pids:?}"));
            kill_process_tree(&pids);
        }
    }
}

/// 把 dsh 子进程的 stdout/stderr 写入 launcher.log。
/// 子进程输出由日志模块统一加时间标签与 [DSH] 标记，因此也会参与 3 天清理，
/// 不再需要单独的 dsh.log 与 1 MiB 清空逻辑。
/// 按读取块切分：遇到换行按行写入；没有换行也随块落盘，
/// 同时保留未完成的 UTF-8 尾字节，避免多字节字符被块边界截断。
fn pump_dsh_output<R: Read + Send + 'static>(mut reader: R) {
    thread::spawn(move || {
        let mut chunk = [0u8; 16 * 1024];
        let mut pending = Vec::new();
        loop {
            let n = match reader.read(&mut chunk) {
                Ok(n) => n,
                Err(e) => {
                    crate::log::error(&format!("读取 dsh 输出失败：{e}"));
                    break;
                }
            };
            if n == 0 {
                break;
            }
            pending.extend_from_slice(&chunk[..n]);

            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=pos).collect();
                log_dsh_bytes(&line);
            }
            flush_partial_dsh_bytes(&mut pending);
        }
        if !pending.is_empty() {
            log_dsh_bytes(&pending);
        }
    });
}

/// 去除行尾 CR/LF 后写入日志。
fn log_dsh_bytes(bytes: &[u8]) {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    crate::log::dsh_output(&String::from_utf8_lossy(&bytes[..end]));
}

/// 把没有换行的块立即落盘，但保留末尾不完整的 UTF-8 序列；
/// 无法继续等待的非法 UTF-8 字节则按 lossy 方式落盘，避免 pending 无限增长。
fn flush_partial_dsh_bytes(pending: &mut Vec<u8>) {
    if pending.is_empty() {
        return;
    }
    let valid_len = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        Err(e) => e.valid_up_to(),
    };
    if valid_len > 0 {
        let valid: Vec<u8> = pending.drain(..valid_len).collect();
        log_dsh_bytes(&valid);
    }
    if pending.is_empty() {
        return;
    }
    if let Err(e) = std::str::from_utf8(pending) {
        if e.error_len().is_some() {
            let invalid = std::mem::take(pending);
            log_dsh_bytes(&invalid);
        }
    }
}

/// 用 Toolhelp 快照递归清理整棵进程树 (仅在 Job 不可用/挂接失败时使用)。
fn kill_child_tree(child: &mut Child) {
    let pid = child.id();
    kill_process_tree(&[pid]);
}

/// 后台隐藏窗口启动 dsh：npx @deepseek-ai/dsh web (工作目录为用户主目录)。
/// 端口覆盖时透传 --port；-y 避免 npx 首次安装的交互确认。
/// cmd/npx/node 的 stdout 与 stderr 由读取线程写入 launcher.log
/// (时间标签 + [DSH] 标记)，与启动器日志合并，不再使用单独的 dsh.log。
/// spawn 后立即挂入全局 Job (KILL_ON_JOB_CLOSE)，再检查退出请求：
/// 任何失败路径都清理整棵进程树，避免 cmd 已派生 npx/node 后留下孤儿。
pub fn start_harness(quitting: &AtomicBool) -> io::Result<()> {
    let home = home_dir();
    let port = web_port();
    let web_cmd = if port == DEFAULT_PORT {
        "npx -y @deepseek-ai/dsh web".to_string()
    } else {
        format!("npx -y @deepseek-ai/dsh web --port {port}")
    };

    let mut child = Command::new("cmd.exe")
        .args(["/c", web_cmd.as_str()])
        .current_dir(&home)
        .creation_flags(CREATE_NO_WINDOW) // 不弹出任何控制台窗口
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // 先挂入 Job，再检查退出请求：确保任何终止动作都覆盖 npx/node 整棵树。
    let job = match job_handle() {
        Some(job) => job,
        None => {
            kill_child_tree(&mut child);
            return Err(io::Error::other(
                "Job 对象不可用 (dsh 将无法被自动回收)，拒绝启动",
            ));
        }
    };
    unsafe {
        use std::os::windows::io::AsRawHandle;
        if AssignProcessToJobObject(job, child.as_raw_handle()) == 0 {
            let err = io::Error::last_os_error();
            crate::log::error(&format!(
                "AssignProcessToJobObject 失败 (dsh 将无法被 Job 回收)：{err}"
            ));
            kill_child_tree(&mut child);
            return Err(err);
        }
    }

    if quitting.load(Ordering::SeqCst) {
        unsafe {
            TerminateJobObject(job, 1);
        }
        return Err(io::Error::new(io::ErrorKind::Interrupted, "收到退出请求"));
    }

    // Job 已挂接成功，取出管道并启动读取线程，避免 cmd/npx/node 因管道写满而阻塞
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("无法获取 dsh stdout 管道"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("无法获取 dsh stderr 管道"))?;
    pump_dsh_output(stdout);
    pump_dsh_output(stderr);

    crate::log::info(&format!(
        "已启动 dsh ({web_cmd}) 并挂入 Job，输出已并入 launcher.log"
    ));
    Ok(())
}

/// 释放单实例互斥体 (退出流程提前调用，让新实例在旧进程退出前即可启动)
pub fn release_single_instance(handle: HANDLE) {
    unsafe {
        CloseHandle(handle);
    }
}

/// 创建命名互斥体实现单实例；返回 None 表示已有实例在运行。
/// 环境变量 DSHLAUNCHER_INSTANCE 可附加互斥体后缀 (测试实例隔离用，
/// 让测试实例与用户实例互不干扰)。
pub fn single_instance_guard() -> Option<HANDLE> {
    // 与日志文件后缀共用同一套清洗规则，避免同名实例却写入不同日志/互斥体
    let suffix = crate::log::instance_id();
    let name_str = format!("Local\\DshLauncher.SingleInstance{suffix}");
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
    fn kill_process_tree_terminates_child_process() {
        let mut child = Command::new("cmd.exe")
            .args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        kill_process_tree(&[child.id()]);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "子进程未在 5 秒内被终止"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn listener_pids_finds_current_process() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let pids = listener_pids(port);
        assert!(
            pids.contains(&std::process::id()),
            "应在监听端口 {port} 的 PID 列表中找到当前进程，实际：{pids:?}"
        );
        drop(listener);
    }

    /// 防回归：应用运行时代码不得通过 PowerShell/netstat/taskkill 代理实现功能。
    /// 只扫描本机源码文本中的精确 Command::new 调用；历史注释中的单词不会误伤。
    #[test]
    fn runtime_source_has_no_shell_proxies() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = [
            root.join("Cargo.toml"),
            root.join("build.rs"),
            root.join("src/main.rs"),
            root.join("src/dsh.rs"),
            root.join("src/log.rs"),
            root.join("src/toast.rs"),
            root.join("src/update.rs"),
        ];
        for path in files {
            let text = std::fs::read_to_string(&path).unwrap();
            for exe in ["powershell", "netstat", "taskkill"] {
                let needle = format!("Command::new(\"{}.exe\")", exe);
                assert!(
                    !text.contains(&needle),
                    "运行时代码禁止代理调用 {exe}.exe：{}",
                    path.display()
                );
            }
            let encoded = format!("-{}", "EncodedCommand");
            assert!(
                !text.contains(&encoded),
                "运行时代码禁止 PowerShell EncodedCommand：{}",
                path.display()
            );
        }
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
