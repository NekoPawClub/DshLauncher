//! 构建脚本：为 Windows 可执行文件嵌入应用图标 (编译 .rc 资源并链接)
//!
//! 不依赖第三方资源编译 crate：自行定位 rc.exe (环境变量 RC_EXE → PATH →
//! Windows Kits → VS 安装目录)，把 icons 目录下的 ICO 编译进 PE 资源。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // 仅 Windows 目标需要嵌入资源
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("缺少 CARGO_MANIFEST_DIR");
    let out_dir = std::env::var("OUT_DIR").expect("缺少 OUT_DIR");

    // 应用图标 (绝对路径，避免 rc.exe 相对路径解析差异)
    let ico_path = Path::new(&manifest_dir)
        .join("icons")
        .join("DeepSeekHarness-WhaleGirl.ico");
    assert!(ico_path.is_file(), "图标文件不存在：{}", ico_path.display());
    // .rc 字符串字面量中反斜杠是转义符 (如 \a、\n)，会损坏路径
    // (CI 路径 D:\a\DshLauncher\... 曾触发 RC2135)；统一转正斜杠
    let ico_rc = ico_path.to_string_lossy().replace('\\', "/");

    // exe 版本四段 (YY.MM.DD.NN)：CI 发布时由环境变量 DSHLAUNCHER_VERSION 传入
    // (如 26.8.18.1)；本地构建回退 Cargo.toml 的 YY.MM.DD + 0。
    let (v1, v2, v3, v4) = exe_version();

    // 动态生成 rc 文件：应用图标 + 版本信息资源 (VS_VERSION_INFO)。
    // 注意：VALUE 字符串用英文，避免 rc.exe 对无 BOM UTF-8 中文的 ANSI 误读乱码
    let rc_content = format!(
        r#"
1 ICON "{ico}"

1 VERSIONINFO
FILEVERSION    {v1},{v2},{v3},{v4}
PRODUCTVERSION {v1},{v2},{v3},{v4}
FILEOS         0x40004
FILETYPE       0x1
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName",      "DshLauncher"
            VALUE "FileDescription",  "DeepSeek Harness Tray Guardian"
            VALUE "FileVersion",      "{v1}.{v2}.{v3}.{v4}"
            VALUE "InternalName",     "DshLauncher"
            VALUE "LegalCopyright",   "Copyright (C) 2026 DshLauncher"
            VALUE "OriginalFilename", "DshLauncher.exe"
            VALUE "ProductName",      "DshLauncher"
            VALUE "ProductVersion",   "{v1}.{v2}.{v3}.{v4}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#,
        ico = ico_rc,
        v1 = v1,
        v2 = v2,
        v3 = v3,
        v4 = v4,
    );
    let rc_file = Path::new(&out_dir).join("app.rc");
    std::fs::write(&rc_file, rc_content).expect("写入 app.rc 失败");

    // 定位 rc.exe
    let rc_exe = find_rc_exe().expect(
        "未找到 rc.exe：请安装 Windows SDK (或 Visual Studio C++ 组件)，         或通过环境变量 RC_EXE 指定 rc.exe 的完整路径",
    );

    // 编译资源：rc /fo app.res app.rc (参数路径统一正斜杠，Windows 兼容)
    let res_file = Path::new(&out_dir).join("app.res");
    let rc_arg = rc_file.to_str().unwrap().replace('\\', "/");
    let res_arg = res_file.to_str().unwrap().replace('\\', "/");
    let status = Command::new(&rc_exe)
        .args(["/fo", &res_arg, &rc_arg])
        .status()
        .expect("rc.exe 启动失败");
    assert!(status.success(), "rc.exe 编译资源失败 (退出码 {status})");

    // 将生成的 .res 交给链接器
    println!("cargo:rustc-link-arg={}", res_file.display());
    // 可重现构建：PE 时间戳归零，同源码产物 hash 稳定 (CI 靠 hash 判断是否需要发布)
    println!("cargo:rustc-link-arg=/Brepro");
    println!("cargo:rerun-if-changed=icons/DeepSeekHarness-WhaleGirl.ico");
    println!("cargo:rerun-if-env-changed=RC_EXE");
    // 版本号变化必须触发重链接，保证产物内嵌版本准确
    println!("cargo:rerun-if-env-changed=DSHLAUNCHER_VERSION");
}

/// exe 版本四段 (YY.MM.DD.NN)：优先环境变量 DSHLAUNCHER_VERSION
/// (CI 发布时传入，如 26.8.18.1)，否则回退 Cargo.toml 的 YY.MM.DD + 0
fn exe_version() -> (u32, u32, u32, u32) {
    if let Ok(v) = std::env::var("DSHLAUNCHER_VERSION") {
        let parts: Vec<u32> = v.split('.').filter_map(|s| s.parse().ok()).collect();
        if parts.len() == 4 {
            return (parts[0], parts[1], parts[2], parts[3]);
        }
    }
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "26.8.18".to_string());
    let parts: Vec<u32> = pkg.split('.').filter_map(|s| s.parse().ok()).collect();
    (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
        0,
    )
}

/// 依次尝试：环境变量 RC_EXE → PATH → Windows Kits → VS 安装目录
fn find_rc_exe() -> Option<PathBuf> {
    // 1) 环境变量显式指定
    if let Ok(path) = std::env::var("RC_EXE") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }

    // 2) PATH 中查找
    if let Ok(output) = Command::new("where.exe").arg("rc.exe").output() {
        if output.status.success() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let p = PathBuf::from(line.trim());
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }

    // 3) Windows Kits 常见位置：bin/<版本>/<架构>/rc.exe，取最新版本、优先 x64
    for kits_root in [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ] {
        if let Ok(entries) = std::fs::read_dir(kits_root) {
            let mut versions: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            // 按版本号数值比较 ("11.0" 必须大于 "9.0")，选最新 SDK
            versions.sort_by_key(|a| version_key(a));
            for version in versions.iter().rev() {
                for arch in ["x64", "x86", "arm64"] {
                    let p = Path::new(kits_root).join(version).join(arch).join("rc.exe");
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }

    // 4) VS 安装目录下递归查找 (兜底)
    for vs_root in [
        r"C:\Program Files\Microsoft Visual Studio",
        r"C:\Program Files (x86)\Microsoft Visual Studio",
    ] {
        if let Ok(entries) = std::fs::read_dir(vs_root) {
            for entry in entries.filter_map(|e| e.ok()) {
                if let Some(found) = find_file_recursive(&entry.path(), "rc.exe", 5) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// 版本字符串 → 数值序列 ("10.0.22621.0" → [10, 0, 22621, 0])，用于正确选取最新 SDK
fn version_key(s: &str) -> Vec<u32> {
    s.split('.').filter_map(|p| p.parse::<u32>().ok()).collect()
}

/// 在目录树下有限深度内查找指定文件名
fn find_file_recursive(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_recursive(&path, name, depth - 1) {
                return Some(found);
            }
        } else if path.file_name().map(|n| n == name).unwrap_or(false) {
            return Some(path);
        }
    }
    None
}
