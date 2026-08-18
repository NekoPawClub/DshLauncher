//! 文档与源码注释只描述当前状态：禁止过程性表述与无出处说法。
//! 本测试不扫描 tests 目录自身，避免测试代码中的拆分字符串误伤。

use std::path::Path;

fn prohibited_words() -> Vec<String> {
    [
        ("不", "再"),
        ("曾", ""),
        ("此", "前"),
        ("原", "先"),
        ("改", "为"),
        ("移", "除"),
        ("新", "增"),
        ("修", "复"),
        ("经", "验"),
        ("本", "次"),
        ("已实", "测"),
        ("按", "审"),
    ]
    .iter()
    .map(|(a, b)| format!("{a}{b}"))
    .collect()
}

#[test]
fn docs_and_source_have_no_process_language() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![
        root.join("Cargo.toml"),
        root.join("build.rs"),
        root.join("README.md"),
        root.join(".github/workflows/ci.yml"),
    ];
    if let Ok(entries) = std::fs::read_dir(root.join("src")) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }

    let words = prohibited_words();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        for word in &words {
            assert!(
                !text.contains(word),
                "{} 包含过程性表述或历史依据用语：{}",
                path.display(),
                word
            );
        }
    }
}
