//! 版本更新检测：轮询 GitHub Releases 最新版本并与本地版本比较。
//!
//! 检测源策略 (用户选定)：GitHub Releases API 直连为主，失败时依次尝试
//! gh-proxy 类镜像前缀转发同一 API。全部源失败视为网络问题，静默返回错误
//! (自动检查不打扰用户；手动检查由调用方反馈)。
//!
//! 实现要点：
//! - WinHTTP (系统组件) 发起 HTTPS：零第三方依赖，自动走系统代理，墙内更稳
//! - 版本号 YY.MM.DD.NN 段长不固定：按 . 分段转数值逐段比较，不能字符串字典序
//! - 响应仅提取 tag_name 与 html_url 两个字段，手工解析 JSON，不引入 serde

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetTimeouts, URL_COMPONENTS, WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE,
};

/// 发布仓库：https://github.com/Antecer/DshLauncher
const API_URL: &str = "https://api.github.com/repos/Antecer/DshLauncher/releases/latest";
/// 镜像前缀候选：主 API 失败后依次尝试 (公共 gh-proxy 实例域名可能失效，逐个静默跳过)
const MIRROR_PREFIXES: &[&str] = &["https://ghproxy.net/", "https://ghfast.top/"];

/// 检测到的新版本信息
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// 版本号 (无 v 前缀)，如 "26.8.18.1"
    pub version: String,
    /// 发布页地址
    pub url: String,
}

/// 本地版本号 (build.rs 经 rustc-env 注入，格式 YY.MM.DD.NN)
pub fn local_version() -> &'static str {
    env!("DSH_LAUNCHER_VERSION")
}

/// 检测最新版本。Ok(Some)=有新版；Ok(None)=已最新；Err=所有源不可达/响应无法解析。
pub fn check_update() -> Result<Option<UpdateInfo>, ()> {
    let local = local_version();
    for url in candidate_urls() {
        let Ok(body) = http_get(&url) else { continue };
        let Some(info) = parse_release_json(&body) else {
            continue;
        };
        crate::log::info(&format!(
            "更新检测成功：远端 v{} (本地 v{local})",
            info.version
        ));
        return Ok(is_newer(local, &info.version).then_some(info));
    }
    Err(())
}

/// 后台检测线程：启动 30 秒后首查 (避开 dsh 拉起期间的网络繁忙)，
/// 之后每 24 小时复查；check_now 置位时立即检查 (菜单手动触发)。
/// 结果经 on_result(manual, result) 回调，回调在检测线程内执行。
pub fn spawn_checker(
    check_now: Arc<AtomicBool>,
    quitting: Arc<AtomicBool>,
    on_result: impl Fn(bool, Result<Option<UpdateInfo>, ()>) + Send + 'static,
) {
    thread::spawn(move || {
        let mut next = Instant::now() + Duration::from_secs(30);
        loop {
            if quitting.load(Ordering::SeqCst) {
                break;
            }
            let manual = check_now.swap(false, Ordering::SeqCst);
            if manual || Instant::now() >= next {
                next = Instant::now() + Duration::from_secs(24 * 60 * 60);
                on_result(manual, check_update());
            }
            thread::sleep(Duration::from_millis(1000));
        }
    });
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

/// 版本解析：YY.MM.DD.NN → 四段数值 (缺段补 0，多段/非数字返回 None)
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

/// 从 GitHub Releases API 响应中提取 tag_name 与 html_url (手工解析，零依赖)
fn parse_release_json(body: &str) -> Option<UpdateInfo> {
    let tag = extract_json_string(body, "tag_name")?;
    let url = extract_json_string(body, "html_url")?;
    let version = tag.trim_start_matches('v').to_string();
    parse_version(&version)?;
    Some(UpdateInfo { version, url })
}

/// 提取 JSON 字符串字段 "key":"value" 的 value，处理常见转义
fn extract_json_string(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let idx = body.find(&pat)? + pat.len();
    let rest = &body[idx..];
    let colon = rest.find(':')? + 1;
    let after = rest[colon..].trim_start();
    let q = after.strip_prefix('"')?;
    let mut i = 0;
    let bytes = q.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'"' {
            break;
        }
        i += 1;
    }
    let raw = &q[..i];
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
        // 目标路径 = path + extra (?query)；所有候选均为 https
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
        let verb = to_wide("GET");
        let request = WinHttpOpenRequest(
            connect,
            verb.as_ptr(),
            object.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
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
        // 循环读取响应体
        let mut body = Vec::new();
        loop {
            let mut size: u32 = 0;
            if WinHttpQueryDataAvailable(request, &mut size) == 0 || size == 0 {
                break;
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
            body.extend_from_slice(&buf[..read as usize]);
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
    fn version_parse_rejects_garbage() {
        assert!(!is_newer("26.8.18.0", "not-a-version"));
        assert!(!is_newer("x", "26.8.18.1"));
    }

    #[test]
    fn release_json_extracts_fields() {
        let body = r#"{
            "url": "https://api.github.com/repos/Antecer/DshLauncher/releases/1",
            "html_url": "https://github.com/Antecer/DshLauncher/releases/tag/v26.8.18.1",
            "id": 1,
            "tag_name": "v26.8.18.1",
            "name": "v26.8.18.1",
            "assets": []
        }"#;
        let info = parse_release_json(body).expect("应解析成功");
        assert_eq!(info.version, "26.8.18.1");
        assert_eq!(
            info.url,
            "https://github.com/Antecer/DshLauncher/releases/tag/v26.8.18.1"
        );
    }

    #[test]
    fn json_escape_handled() {
        let body = r#"{"tag_name":"v26.8.18.1","html_url":"https:\/\/github.com\/x"}"#;
        let info = parse_release_json(body).expect("转义应正确处理");
        assert_eq!(info.url, "https://github.com/x");
    }
}
