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
    // (CI 路径 D:/a/DshLauncher/... 曾触发 RC2135)；统一转正斜杠
    let ico_rc = ico_path.to_string_lossy().replace('\\', "/");

    // exe 版本四段 (YY.MM.DD.NN)：CI 发布时由环境变量 DSHLAUNCHER_VERSION 传入
    // (如 26.8.18.1)；本地构建回退为构建当天的本地日期 + 0。
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
            VALUE "FileVersion",      "{v1:02}.{v2:02}.{v3:02}.{v4:02}"
            VALUE "InternalName",     "DshLauncher"
            VALUE "LegalCopyright",   "Copyright (C) 2026 DshLauncher"
            VALUE "OriginalFilename", "DshLauncher.exe"
            VALUE "ProductName",      "DshLauncher"
            VALUE "ProductVersion",   "{v1:02}.{v2:02}.{v3:02}.{v4:02}"
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

    // 运行时版本常量：update 模块据此与远端最新版本比较
    // (四段固定两位补零，如 26.08.05.01：字符串序即版本序)
    println!("cargo:rustc-env=DSH_LAUNCHER_VERSION={v1:02}.{v2:02}.{v3:02}.{v4:02}");
    // 将生成的 .res 交给链接器
    println!("cargo:rustc-link-arg={}", res_file.display());
    // 可重现构建：PE 时间戳归零，同源码产物 hash 稳定；
    // 发布判定由 CI 的源码 diff 完成，不依赖产物 hash。
    println!("cargo:rustc-link-arg=/Brepro");
    println!("cargo:rerun-if-changed=icons/DeepSeekHarness-WhaleGirl.ico");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-env-changed=RC_EXE");
    // 版本号变化必须触发重链接，保证产物内嵌版本准确
    println!("cargo:rerun-if-env-changed=DSHLAUNCHER_VERSION");
}

/// exe 版本四段 (YY.MM.DD.NN)：CI 发布时由环境变量 DSHLAUNCHER_VERSION 传入
/// (如 26.8.18.1)；本地构建回退为 build.rs 本次执行时的本地日期 + 0。
///
/// 版本只在 build.rs 重新执行时才会取新日期；build.rs 仅声明源码/图标/版本环境
/// 为输入，不会因“只跨天”而重跑，因此源码未变的重复构建会维持旧版本，
/// 符合 CI “源码无实质变化不发布”的判定逻辑。
fn exe_version() -> (u32, u32, u32, u32) {
    if let Ok(v) = std::env::var("DSHLAUNCHER_VERSION") {
        let parts: Vec<u32> = v.split('.').filter_map(|s| s.parse().ok()).collect();
        if parts.len() == 4 {
            return (parts[0], parts[1], parts[2], parts[3]);
        }
    }
    let (yy, mm, dd) = local_date();
    (yy, mm, dd, 0)
}

/// 当前本地日期 (YY, MM, DD)：kernel32!GetLocalTime，不引入第三方日期依赖。
/// 仅在 build.rs 被 Cargo 判定需要重跑时调用；跨天但源码未变不会调用。
fn local_date() -> (u32, u32, u32) {
    #[repr(C)]
    struct SystemTimeLocal {
        w_year: u16,
        w_month: u16,
        w_day_of_week: u16,
        w_day: u16,
        w_hour: u16,
        w_minute: u16,
        w_second: u16,
        w_milliseconds: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(st: *mut SystemTimeLocal);
    }
    let mut st = SystemTimeLocal {
        w_year: 0,
        w_month: 0,
        w_day_of_week: 0,
        w_day: 0,
        w_hour: 0,
        w_minute: 0,
        w_second: 0,
        w_milliseconds: 0,
    };
    // SAFETY: st 为栈上有效指针，GetLocalTime 写入本地系统时间
    unsafe { GetLocalTime(&mut st) };
    ((st.w_year % 100) as u32, st.w_month as u32, st.w_day as u32)
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

    // 3) Windows Kits 常见位置：bin/<版本>/<架构>/rc.exe，取最新版本、优先主机架构
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
                for arch in preferred_archs() {
                    let p = Path::new(kits_root).join(version).join(arch).join("rc.exe");
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }

    // 4) VS 安装目录：按标准固定相对布局精确匹配，不做全盘递归。
    // VS 的相对路径是安装器固定的，用户不会修改：
    // <VS root>/<year>/<edition>/VC/Tools/MSVC/<toolset>/bin/Host<host>/<target>/rc.exe
    find_vs_rc_exe()
}

/// 按 VS 标准安装布局收集 rc.exe：
/// 仅枚举两个可变层级 (edition 与 MSVC 工具集版本)，其余路径字段固定拼接。
/// 同一工具集内优先 Host<主机架构>/<主机架构>，其次主机架构下的其它 target，
/// 最后其它 host；跨工具集版本优先取新版本。
fn find_vs_rc_exe() -> Option<PathBuf> {
    let archs = preferred_archs();
    let mut candidates: Vec<(usize, usize, Vec<u32>, PathBuf)> = Vec::new();
    for vs_root in [
        r"C:\Program Files\Microsoft Visual Studio",
        r"C:\Program Files (x86)\Microsoft Visual Studio",
    ] {
        let Ok(years) = std::fs::read_dir(vs_root) else {
            continue;
        };
        for year in years.filter_map(|e| e.ok()) {
            let Ok(editions) = std::fs::read_dir(year.path()) else {
                continue;
            };
            for edition in editions.filter_map(|e| e.ok()) {
                let msvc = edition.path().join("VC").join("Tools").join("MSVC");
                let Ok(entries) = std::fs::read_dir(&msvc) else {
                    continue;
                };
                let mut versions: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.path())
                    .collect();
                versions.sort_by_key(|p| {
                    version_key(p.file_name().and_then(|n| n.to_str()).unwrap_or(""))
                });
                for version in versions.iter().rev() {
                    for (host_rank, host) in archs.iter().enumerate() {
                        for (target_rank, target) in archs.iter().enumerate() {
                            let p = version
                                .join("bin")
                                .join(format!("Host{host}"))
                                .join(target)
                                .join("rc.exe");
                            if p.is_file() {
                                candidates.push((
                                    host_rank,
                                    target_rank,
                                    version_key(
                                        version.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                                    ),
                                    p,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    candidates.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)).then_with(|| b.2.cmp(&a.2)));
    candidates.into_iter().next().map(|c| c.3)
}

/// 版本字符串 → 数值序列 ("10.0.22621.0" → [10, 0, 22621, 0])，用于正确选取最新 SDK
fn version_key(s: &str) -> Vec<u32> {
    s.split('.').filter_map(|p| p.parse::<u32>().ok()).collect()
}

/// 主机架构映射为 Windows Kits 目录名。
fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "x86",
        "aarch64" => "arm64",
        _ => "x64",
    }
}

/// 返回 rc.exe 架构目录的查找顺序：主机架构优先，其余作为兜底。
fn preferred_archs() -> Vec<&'static str> {
    let host = host_arch();
    let mut archs = vec![host];
    for arch in ["x64", "x86", "arm64"] {
        if arch != host {
            archs.push(arch);
        }
    }
    archs
}
