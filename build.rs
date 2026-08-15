//! 构建脚本：为 Windows 可执行文件嵌入应用图标（编译 .rc 资源并链接）
//!
//! 不依赖第三方资源编译 crate：自行定位 rc.exe（环境变量 RC_EXE → PATH →
//! Windows Kits → VS 安装目录），把 Icons 目录下的 ICO 编译进 PE 资源。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // 仅 Windows 目标需要嵌入资源
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("缺少 CARGO_MANIFEST_DIR");
    let out_dir = std::env::var("OUT_DIR").expect("缺少 OUT_DIR");

    // 应用图标（绝对路径，避免 rc.exe 相对路径解析差异）
    let ico_path = Path::new(&manifest_dir)
        .join("Icons")
        .join("DeepSeekHarness-WhaleGirl.ico");
    assert!(ico_path.is_file(), "图标文件不存在：{}", ico_path.display());

    // 动态生成 rc 文件（含图标绝对路径）
    let rc_file = Path::new(&out_dir).join("app.rc");
    std::fs::write(&rc_file, format!("1 ICON \"{}\"\n", ico_path.display()))
        .expect("写入 app.rc 失败");

    // 定位 rc.exe
    let rc_exe = find_rc_exe().expect(
        "未找到 rc.exe：请安装 Windows SDK（或 Visual Studio C++ 组件），         或通过环境变量 RC_EXE 指定 rc.exe 的完整路径",
    );

    // 编译资源：rc /fo app.res app.rc
    let res_file = Path::new(&out_dir).join("app.res");
    let status = Command::new(&rc_exe)
        .args(["/fo", res_file.to_str().unwrap(), rc_file.to_str().unwrap()])
        .status()
        .expect("rc.exe 启动失败");
    assert!(status.success(), "rc.exe 编译资源失败（退出码 {status}）");

    // 将生成的 .res 交给链接器
    println!("cargo:rustc-link-arg={}", res_file.display());
    println!("cargo:rerun-if-changed=Icons/DeepSeekHarness-WhaleGirl.ico");
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
            versions.sort();
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

    // 4) VS 安装目录下递归查找（兜底）
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
