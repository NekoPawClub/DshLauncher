//! 版本更新检测：轮询 GitHub Releases 最新版本并与本地版本比较。
//!
//! 检测源策略 (用户选定)：GitHub Releases API 直连为主，失败时依次尝试
//! gh-proxy 类镜像前缀转发同一 API。全部源失败视为网络问题，静默返回错误。
//!
//! 提示策略：发现更新后由 Rust 直接通过 WinRT 发送 Windows toast (不启动 PowerShell)；
//! 去重依据为 launcher.log 中最近一条仍在 3 天保留窗口内的“更新通知成功”日志行，
//! 检测成功行只用于记录检查结果。通知失败只写 FAIL 日志，不写成功记录，
//! 同一版本在保留窗口内不重复提示，出现更新的版本后再提示。
//!
//! 实现要点：
//! - WinHTTP (系统组件) 发起 HTTPS：零第三方 HTTP 依赖，自动走系统代理，墙内更稳
//! - 版本号 YY.MM.DD.NN 段长不固定：按 . 分段转数值逐段比较，不能字符串字典序
//! - 响应仅提取 tag_name 字段，手工解析 JSON，不引入 serde

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts, URL_COMPONENTS,
    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2,
    WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3, WINHTTP_OPTION_SECURE_PROTOCOLS,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

/// 发布仓库：https://github.com/NekoPawClub/DshLauncher
const API_URL: &str = "https://api.github.com/repos/NekoPawClub/DshLauncher/releases/latest";
/// 镜像前缀候选：主 API 失败后依次尝试 (公共 gh-proxy 实例域名可能失效，逐个静默跳过)
const MIRROR_PREFIXES: &[&str] = &["https://ghproxy.net/", "https://ghfast.top/"];

/// 检测到的新版本信息
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// 版本号 (无 v 前缀)，如 "26.08.18.01"
    pub version: String,
}

/// 日志日 (凌晨 4 点为日界)：年、月、日
type LogDay = (i64, u32, u32);

/// 本地版本号 (build.rs 经 rustc-env 注入，格式 YY.MM.DD.NN)
pub fn local_version() -> &'static str {
    env!("DSH_LAUNCHER_VERSION")
}

/// 获取远端最新版本。Ok=成功获取并解析；Err=所有源不可达/响应无法解析。
/// 是否“有更新”由调用方与本地版本比较决定；成功结果是否写日志也由调用方控制
/// (启动即写一次，之后仅跨凌晨 4 点日志日或远端出现更新版本时再写)。
fn fetch_latest_version() -> Result<UpdateInfo, ()> {
    for url in candidate_urls() {
        let Ok(body) = http_get(&url) else { continue };
        let Some(info) = parse_release_json(&body) else {
            continue;
        };
        return Ok(info);
    }
    Err(())
}

/// 是否通过环境变量禁用更新检测 (开发/测试用)。
/// 支持 1/true/yes/on (大小写不敏感)；未设置或其它值视为启用。
fn update_check_disabled() -> bool {
    std::env::var("DSHLAUNCHER_UPDATE_DISABLE")
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// 后台检测线程：启动即首查一次，之后每 1 小时复查。
/// 成功结果按日志日写入 launcher.log：进程启动写一次；运行跨过凌晨 4 点后写一次；
/// 同一日志日内只有远端出现更新版本时才再写，避免每小时重复刷日志。
/// 发现新版本且最近日志中未记录过该版本时直接通过 WinRT 发 toast。
/// 发送失败后由下一轮检查重试；发送成功后同一版本在保留窗口内不重复提示。
pub fn spawn_checker(quitting: Arc<AtomicBool>) {
    if update_check_disabled() {
        crate::log::info("更新检测已禁用 (DSHLAUNCHER_UPDATE_DISABLE)");
        return;
    }
    thread::spawn(move || {
        let mut next = Instant::now();
        let mut last_written_remote: Option<String> = None;
        let mut last_written_day: Option<LogDay> = None;
        let mut last_notified_remote: Option<String> = None;
        let mut notify_failed_version: Option<String> = None;
        loop {
            if quitting.load(Ordering::SeqCst) {
                break;
            }
            if Instant::now() >= next {
                next = Instant::now() + Duration::from_secs(60 * 60);
                run_check(
                    &mut last_written_remote,
                    &mut last_written_day,
                    &mut last_notified_remote,
                    &mut notify_failed_version,
                );
            }
            thread::sleep(Duration::from_millis(1000));
        }
    });
}

/// 单次检测：检测成功日志按日写入；通知成功日志单独作为跨进程去重依据。
/// 通知失败只写 FAIL 日志，下一轮在本进程内重试；进程重启后日志中没有
/// 该版本的通知成功记录，仍会再次尝试提示。
fn run_check(
    last_written_remote: &mut Option<String>,
    last_written_day: &mut Option<LogDay>,
    last_notified_remote: &mut Option<String>,
    notify_failed_version: &mut Option<String>,
) {
    match fetch_latest_version() {
        Ok(info) => {
            let local = local_version();

            if is_newer(local, &info.version) {
                let retry_failed = notify_failed_version.as_deref() == Some(info.version.as_str());
                let last_logged_notified = crate::log::last_logged_notify_version();
                let last_known = newest_version(
                    last_logged_notified.as_deref(),
                    last_notified_remote.as_deref(),
                );
                if retry_failed || should_notify_version(&info.version, last_known) {
                    match crate::toast::show_update_toast(&info.version) {
                        Ok(()) => {
                            *notify_failed_version = None;
                            *last_notified_remote = Some(info.version.clone());
                            crate::log::info(&format!("更新通知成功：远端 v{}", info.version));
                        }
                        Err(e) => {
                            *notify_failed_version = Some(info.version.clone());
                            crate::log::error(&format!(
                                "发送更新通知失败 (v{})：{e}，下一轮检查将重试",
                                info.version
                            ));
                        }
                    }
                }
            } else if notify_failed_version.as_deref() == Some(info.version.as_str()) {
                // 远端版本不高于本地时，清除遗留的失败重试状态
                *notify_failed_version = None;
            }

            let today = crate::log::current_log_day();
            if should_log_update_check(
                &info.version,
                last_written_remote.as_deref(),
                *last_written_day,
                today,
            ) {
                crate::log::info(&format!(
                    "更新检测成功：远端 v{} (本地 v{local})",
                    info.version
                ));
                *last_written_remote = Some(info.version);
                *last_written_day = Some(today);
            }
        }
        Err(()) => crate::log::info("更新检测失败 (网络不可达)，静默忽略"),
    }
}

/// 是否需要提示：远端版本 > 最近日志中的通知成功版本 → 提示；否则不提示
fn should_notify_version(remote: &str, last_logged: Option<&str>) -> bool {
    match last_logged {
        Some(last) => is_newer(last, remote),
        None => true,
    }
}

/// 取两个候选版本中较新的一个 (用于合并通知成功日志与本进程内存状态)
fn newest_version<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if is_newer(a, b) { b } else { a }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// 是否需要把当前在线版本写入日志：
/// - 本进程首次检查 → 写 (对应“程序启动写一次”)
/// - 已跨日志日 (凌晨 4 点日界) → 写 (对应“每天重新写一次”)
/// - 同一日志日内远端出现更新版本 → 写 (对应“有新版本才写”)
/// - 同一日志日内版本未变 → 不写
fn should_log_update_check(
    remote: &str,
    last_written_remote: Option<&str>,
    last_written_day: Option<LogDay>,
    today: LogDay,
) -> bool {
    if last_written_day != Some(today) {
        return true;
    }
    match last_written_remote {
        Some(last) => is_newer(last, remote),
        None => true,
    }
}

/// 候选 URL：主 API 优先；环境变量 DSHLAUNCHER_UPDATE_MIRROR 可自定义镜像前缀
/// (逗号分隔)，置于内置候选之前
fn candidate_urls() -> Vec<String> {
    let mut urls = vec![API_URL.to_string()];
    if let Ok(custom) = std::env::var("DSHLAUNCHER_UPDATE_MIRROR") {
        for m in custom.split(',') {
            let m = m.trim();
            if !m.is_empty() {
                urls.push(format!("{m}{API_URL}"));
            }
        }
    }
    for p in MIRROR_PREFIXES {
        urls.push(format!("{p}{API_URL}"));
    }
    urls
}

/// 版本解析：YY.MM.DD.NN → 四段数值 (缺段补 0，多段/非数字返回 None)。
/// 各段固定两位补零 (如 26.08.05.01)，历史版本可能未补零，解析按数值归一化
fn parse_version(v: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = [0u32; 4];
    let segs: Vec<&str> = v.trim().trim_start_matches('v').split('.').collect();
    if segs.is_empty() || segs.len() > 4 {
        return None;
    }
    for (i, s) in segs.iter().enumerate() {
        parts[i] = s.parse().ok()?;
    }
    Some((parts[0], parts[1], parts[2], parts[3]))
}

/// 远端是否比本地新 (按段数值比较)
pub fn is_newer(local: &str, remote: &str) -> bool {
    match (parse_version(local), parse_version(remote)) {
        (Some(a), Some(b)) => (b.0, b.1, b.2, b.3) > (a.0, a.1, a.2, a.3),
        _ => false,
    }
}

/// 从 GitHub Releases API 响应中提取 tag_name (手工解析，零依赖)
fn parse_release_json(body: &str) -> Option<UpdateInfo> {
    let tag = extract_json_string(body, "tag_name")?;
    let version = tag.trim_start_matches('v').to_string();
    parse_version(&version)?;
    Some(UpdateInfo { version })
}

/// 提取 JSON 字符串字段 "key":"value" 的 value，处理常见转义。
/// 畸形 JSON (如孤立反斜杠/缺少闭合引号) 返回 None，不让解析线程 panic。
fn extract_json_string(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let idx = body.find(&pat)? + pat.len();
    let rest = &body[idx..];
    let colon = rest.find(':')? + 1;
    let after = rest[colon..].trim_start();
    let q = after.strip_prefix('"')?;

    // 用字符边界扫描，找到未转义的闭合引号；反斜杠后的任意字符都安全跳过。
    let mut escaped = false;
    let mut end = None;
    for (idx, c) in q.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '"' {
            end = Some(idx);
            break;
        }
    }
    let raw = &q[..end?];

    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                match n {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    other => out.push(other),
                }
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// 响应体上限：GitHub Releases JSON 远小于该值；防止异常镜像返回超大 body。
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// WinHTTP GET 请求：返回响应体文本；任何失败返回 Err。
/// 走系统代理 (DEFAULT_PROXY)，各阶段超时见 WinHttpSetTimeouts。
fn http_get(url: &str) -> Result<String, ()> {
    let ua = to_wide("DshLauncher-updater/1.0");
    let url_w = to_wide(url);
    unsafe {
        let session = WinHttpOpen(
            ua.as_ptr(),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        if session.is_null() {
            return Err(());
        }
        // HTTPS 请求显式限定 TLS 1.2/1.3；旧系统不支持 1.3 时回退到仅 1.2。
        if url.to_ascii_lowercase().starts_with("https://") {
            let mut protocols =
                WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
            if WinHttpSetOption(
                session,
                WINHTTP_OPTION_SECURE_PROTOCOLS,
                &protocols as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            ) == 0
            {
                protocols = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2;
                if WinHttpSetOption(
                    session,
                    WINHTTP_OPTION_SECURE_PROTOCOLS,
                    &protocols as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<u32>() as u32,
                ) == 0
                {
                    WinHttpCloseHandle(session);
                    return Err(());
                }
            }
        }
        // 解析 URL：预分配缓冲供 WinHttpCrackUrl 填充
        let mut scheme_buf = [0u16; 16];
        let mut host_buf = [0u16; 256];
        let mut path_buf = [0u16; 4096];
        let mut extra_buf = [0u16; 1024];
        let mut comp = URL_COMPONENTS {
            dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
            lpszScheme: scheme_buf.as_mut_ptr(),
            dwSchemeLength: scheme_buf.len() as u32,
            nScheme: 0,
            lpszHostName: host_buf.as_mut_ptr(),
            dwHostNameLength: host_buf.len() as u32,
            nPort: 0,
            lpszUserName: std::ptr::null_mut(),
            dwUserNameLength: 0,
            lpszPassword: std::ptr::null_mut(),
            dwPasswordLength: 0,
            lpszUrlPath: path_buf.as_mut_ptr(),
            dwUrlPathLength: path_buf.len() as u32,
            lpszExtraInfo: extra_buf.as_mut_ptr(),
            dwExtraInfoLength: extra_buf.len() as u32,
        };
        if WinHttpCrackUrl(url_w.as_ptr(), url_w.len() as u32, 0, &mut comp) == 0 {
            WinHttpCloseHandle(session);
            return Err(());
        }
        // 单请求超时：解析 3s / 连接 5s / 发送 8s / 接收 8s
        let _ = WinHttpSetTimeouts(session, 3000, 5000, 8000, 8000);
        let connect = WinHttpConnect(session, comp.lpszHostName, comp.nPort, 0);
        if connect.is_null() {
            WinHttpCloseHandle(session);
            return Err(());
        }
        // 目标路径 = path + extra (?query)；根据 URL scheme 选择是否启用 TLS
        let path_len = comp.dwUrlPathLength as usize;
        let extra_len = comp.dwExtraInfoLength as usize;
        let mut object = Vec::with_capacity(path_len + extra_len + 2);
        if path_len == 0 {
            object.push(u16::from(b'/'));
        } else {
            object.extend_from_slice(&path_buf[..path_len]);
        }
        object.extend_from_slice(&extra_buf[..extra_len]);
        object.push(0);
        let secure = if url.to_ascii_lowercase().starts_with("https://") {
            WINHTTP_FLAG_SECURE
        } else {
            0
        };
        let verb = to_wide("GET");
        let request = WinHttpOpenRequest(
            connect,
            verb.as_ptr(),
            object.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            secure,
        );
        if request.is_null() {
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return Err(());
        }
        let ok = WinHttpSendRequest(request, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) != 0
            && WinHttpReceiveResponse(request, std::ptr::null_mut()) != 0;
        if !ok {
            WinHttpCloseHandle(request);
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return Err(());
        }
        // 校验 HTTP 状态码：只有 2xx 响应才读取，404/429/镜像错误页直接切换下一候选。
        let mut status: u32 = 0;
        let mut status_len = std::mem::size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            &mut status as *mut _ as *mut core::ffi::c_void,
            &mut status_len,
            std::ptr::null_mut(),
        ) == 0
            || !(200..300).contains(&status)
        {
            WinHttpCloseHandle(request);
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return Err(());
        }
        // 循环读取响应体，并限制单块与累计大小。
        let mut body = Vec::new();
        loop {
            let mut size: u32 = 0;
            if WinHttpQueryDataAvailable(request, &mut size) == 0 || size == 0 {
                break;
            }
            if size as usize > MAX_RESPONSE_BYTES {
                WinHttpCloseHandle(request);
                WinHttpCloseHandle(connect);
                WinHttpCloseHandle(session);
                return Err(());
            }
            let mut buf = vec![0u8; size as usize];
            let mut read: u32 = 0;
            if WinHttpReadData(
                request,
                buf.as_mut_ptr() as *mut std::ffi::c_void,
                buf.len() as u32,
                &mut read,
            ) == 0
                || read == 0
            {
                break;
            }
            let read = read as usize;
            if body.len().saturating_add(read) > MAX_RESPONSE_BYTES {
                WinHttpCloseHandle(request);
                WinHttpCloseHandle(connect);
                WinHttpCloseHandle(session);
                return Err(());
            }
            body.extend_from_slice(&buf[..read]);
        }
        WinHttpCloseHandle(request);
        WinHttpCloseHandle(connect);
        WinHttpCloseHandle(session);
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

/// 字符串 → 带终止符 UTF-16 (WinHTTP 各参数用)
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_segment_numeric() {
        assert!(is_newer("26.8.18.0", "26.8.18.1"));
        assert!(is_newer("26.8.18.1", "26.8.19.1"));
        assert!(is_newer("26.8.18.5", "26.9.1.0"));
        assert!(!is_newer("26.8.18.1", "26.8.18.1"));
        // 段长不固定：10 > 9，字符串字典序会误判，数值比较必须正确
        assert!(!is_newer("26.10.1.0", "26.9.30.9"));
        assert!(is_newer("26.9.30.9", "26.10.1.0"));
    }

    #[test]
    fn local_version_injected_by_build_script() {
        // build.rs 经 rustc-env 注入版本号：编译通过已证明环境变量存在，此处断言格式合法
        assert!(
            parse_version(local_version()).is_some(),
            "本地版本格式非法：{}",
            local_version()
        );
    }

    #[test]
    fn notify_dedupe_by_logged_notify_version() {
        // 首次 (日志中无通知成功版本记录) → 提示
        assert!(should_notify_version("26.08.18.01", None));
        // 同版本重复 → 不提示
        assert!(!should_notify_version("26.08.18.01", Some("26.08.18.01")));
        // 出现更新的版本 → 再提示
        assert!(should_notify_version("26.08.18.02", Some("26.08.18.01")));
        // 回退到旧版本 → 不提示
        assert!(!should_notify_version("26.08.18.01", Some("26.08.18.02")));

        // 通知成功日志与本进程内存状态取较新者，日志写入失败时仍可抑制重复提示
        assert_eq!(
            newest_version(None, Some("26.08.18.01")),
            Some("26.08.18.01")
        );
        assert_eq!(
            newest_version(Some("26.08.18.01"), Some("26.08.18.02")),
            Some("26.08.18.02")
        );
        assert!(!should_notify_version(
            "26.08.18.02",
            newest_version(Some("26.08.18.01"), Some("26.08.18.02"))
        ));
    }

    #[test]
    fn update_check_logged_once_per_startup_or_log_day() {
        let today = (2026, 8, 18);
        let next_day = (2026, 8, 19);

        // 进程首次检查 → 写日志
        assert!(should_log_update_check("26.08.18.01", None, None, today));
        // 同一日志日版本未变 → 不写
        assert!(!should_log_update_check(
            "26.08.18.01",
            Some("26.08.18.01"),
            Some(today),
            today
        ));
        // 同一日志日出现更新版本 → 写
        assert!(should_log_update_check(
            "26.08.18.02",
            Some("26.08.18.01"),
            Some(today),
            today
        ));
        // 同一日志日回退到旧版本 → 不写
        assert!(!should_log_update_check(
            "26.08.18.01",
            Some("26.08.18.02"),
            Some(today),
            today
        ));
        // 跨过日志日 (凌晨 4 点日界) → 即使版本未变也写一次
        assert!(should_log_update_check(
            "26.08.18.02",
            Some("26.08.18.02"),
            Some(today),
            next_day
        ));
    }

    #[test]
    fn version_padded_segments_normalized() {
        // 补零格式与历史非补零格式数值等价
        assert_eq!(parse_version("26.08.05.01"), Some((26, 8, 5, 1)));
        assert!(is_newer("26.8.5.0", "26.08.05.01"));
    }

    #[test]
    fn version_parse_rejects_garbage() {
        assert!(!is_newer("26.8.18.0", "not-a-version"));
        assert!(!is_newer("x", "26.8.18.1"));
    }

    #[test]
    fn release_json_extracts_fields() {
        let body = r#"{
            "url": "https://api.github.com/repos/NekoPawClub/DshLauncher/releases/1",
            "html_url": "https://github.com/NekoPawClub/DshLauncher/releases/tag/v26.8.18.1",
            "id": 1,
            "tag_name": "v26.8.18.1",
            "name": "v26.8.18.1",
            "assets": []
        }"#;
        let info = parse_release_json(body).expect("应解析成功");
        assert_eq!(info.version, "26.8.18.1");
    }

    #[test]
    fn json_escape_handled() {
        // extract_json_string 对常见 JSON 转义的处理 (如 \/ 反斜杠斜杠)
        let body = r#"{"html_url":"https:\/\/github.com\/x","tag_name":"v26.8.18.1"}"#;
        assert_eq!(
            extract_json_string(body, "html_url").as_deref(),
            Some("https://github.com/x")
        );
    }

    #[test]
    fn update_disable_flag_parses_known_values() {
        std::env::set_var("DSHLAUNCHER_UPDATE_DISABLE", "yes");
        assert!(update_check_disabled());
        std::env::set_var("DSHLAUNCHER_UPDATE_DISABLE", "0");
        assert!(!update_check_disabled());
        std::env::remove_var("DSHLAUNCHER_UPDATE_DISABLE");
        assert!(!update_check_disabled());
    }

    #[test]
    fn json_malformed_string_returns_none() {
        // 孤立反斜杠 + 缺失闭合引号：解析器应返回 None，而不是 panic。
        assert_eq!(
            extract_json_string(r#"{"tag_name":"v26.8.18.1"#, "tag_name"),
            None
        );
        assert_eq!(extract_json_string(r#"{"tag_name":v1}"#, "tag_name"), None);
    }
}
