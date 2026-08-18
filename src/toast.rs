//! Windows 更新通知：直接通过 WinRT (windows crate) 发送系统 toast。
//!
//! 发送前用 windows-sys 在 HKCU 登记自有 AUMID
//! (NekoPawClub.DshLauncher：显示名 DshLauncher + exe 内嵌图标)，
//! 通知中心因此显示为 DshLauncher。整个过程不再启动 PowerShell 子进程。

use std::io;

use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY_CURRENT_USER, KEY_SET_VALUE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};

/// 通知中心里显示的应用标识
const AUMID: &str = "NekoPawClub.DshLauncher";
/// 点击通知打开的下载页面
const RELEASE_URL: &str = "https://github.com/NekoPawClub/DshLauncher/releases";

/// 发送“发现新版本 vX”的 Windows toast。
/// 返回 Err 表示注册表写入、XML 解析或 WinRT 调用任一步骤失败。
pub fn show_update_toast(version: &str) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("获取当前 exe 路径失败：{e}"))?;
    register_aumid(&file_uri(&exe)).map_err(|e| format!("注册 AUMID 失败：{e}"))?;

    let xml = XmlDocument::new().map_err(|e| format!("创建 XmlDocument 失败：{e}"))?;
    xml.LoadXml(&HSTRING::from(toast_xml(version)))
        .map_err(|e| format!("解析 toast XML 失败：{e}"))?;
    let toast = ToastNotification::CreateToastNotification(&xml)
        .map_err(|e| format!("创建 ToastNotification 失败：{e}"))?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))
        .map_err(|e| format!("创建 ToastNotifier 失败：{e}"))?;
    notifier
        .Show(&toast)
        .map_err(|e| format!("显示 toast 失败：{e}"))?;
    Ok(())
}

/// 生成 toast XML；version 只来自 GitHub tag 解析出的数字/点版本号，XML 安全。
fn toast_xml(version: &str) -> String {
    format!(
        r#"<toast activationType="protocol" launch="{RELEASE_URL}"><visual><binding template="ToastGeneric"><text>DshLauncher 新版本 v{version}</text><text>点击通知打开下载页面</text></binding></visual></toast>"#
    )
}

/// 在 HKCU 登记/更新自有 AUMID (无需管理员权限，幂等)。
fn register_aumid(icon_uri: &str) -> io::Result<()> {
    let subkey = to_wide(&format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
    let mut key = std::ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if key.is_null() {
        return Err(io::Error::other("RegCreateKeyExW 未返回注册表句柄"));
    }

    let display = set_reg_string(key, "DisplayName", "DshLauncher");
    let icon = set_reg_string(key, "IconUri", icon_uri);
    unsafe { RegCloseKey(key) };
    display.and(icon)
}

/// 写入 REG_SZ 值 (UTF-16LE + 终止 NUL)。
fn set_reg_string(key: *mut core::ffi::c_void, name: &str, value: &str) -> io::Result<()> {
    let name_w = to_wide(name);
    let value_w = to_wide(value);
    let status = unsafe {
        RegSetValueExW(
            key,
            name_w.as_ptr(),
            0,
            REG_SZ,
            value_w.as_ptr() as *const u8,
            (value_w.len() * 2) as u32,
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

/// 把 Windows 路径转为 file URI；除 URI 非保留字符外一律百分号编码。
fn file_uri(path: &str) -> String {
    let mut out = String::from("file:///");
    for &b in path.replace('\\', "/").as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 字符串 → 带终止符 UTF-16 (Win32 注册表 API 用)
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_xml_uses_own_aumid_and_protocol_activation() {
        let xml = toast_xml("26.08.18.01");
        assert!(xml.contains("DshLauncher 新版本 v26.08.18.01"));
        assert!(xml.contains("activationType=\"protocol\""));
        assert!(xml.contains("launch=\"https://github.com/NekoPawClub/DshLauncher/releases\""));
    }

    #[test]
    fn file_uri_encodes_spaces_and_reserved_chars() {
        assert_eq!(
            file_uri(r"C:\Users\O'Brien\Dsh Launcher.exe"),
            "file:///C:/Users/O%27Brien/Dsh%20Launcher.exe"
        );
    }

    #[test]
    fn to_wide_encodes_and_terminates() {
        assert_eq!(to_wide("ab中"), vec![0x61, 0x62, 0x4e2d, 0]);
    }
}
