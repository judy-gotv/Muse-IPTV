//! cargo-xtask 开发辅助命令（开发文档 §12.8）。
//!
//! - `cargo xtask i18n-check`：检查 `locales/` 下 5 种语言 + `en-XA` 的
//!   `.ftl` key 是否齐全（缺 key / 多余 key / 占位符 `$var` 不一致都报错），
//!   并扫描 `crates/` 里 `t_or(`/`t_args_or(` 的引用与 FTL key 集比对
//!   （M11：只做 FTL 间一致性时，代码引用的缺 key 也能全绿）。
//! - `cargo xtask check-all`：依次跑 `fmt --check`、clippy、test。
//!
//! 仅用标准库，不依赖业务 crate。
// 作者: Passwd Word

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const LOCALES: &[&str] = &["zh-Hant-TW", "zh-Hans-CN", "en", "ja", "ko", "en-XA"];
const FTL_FILES: &[&str] = &["common.ftl", "portal.ftl", "admin.ftl", "errors.ftl"];

fn workspace_root() -> PathBuf {
    // xtask 的 CARGO_MANIFEST_DIR 是 <root>/xtask。
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// 一个 .ftl 文件解析后的 key -> 占位符集合，并记录重复 key。
#[derive(Default)]
struct FtlFile {
    keys: BTreeMap<String, BTreeSet<String>>,
    duplicates: Vec<String>,
}

/// 解析 .ftl：行首 `key = value` 为新 key；缩进的续行（Fluent 选择器等多行值）
/// 归属上一个 key；`{ $var }` / `{ DATETIME($since, ...) }` 里的 `$var` 记为占位符。
fn parse_ftl(text: &str) -> FtlFile {
    let mut out = FtlFile::default();
    let mut current: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        // 新 key：行首标识符 + `=`（Fluent 标识符以字母开头）。
        let is_key_line = !line.starts_with(|c: char| c.is_whitespace())
            && line
                .split_once('=')
                .map(|(k, _)| {
                    let k = k.trim();
                    !k.is_empty()
                        && k.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                        && k.chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
                })
                .unwrap_or(false);
        if is_key_line {
            let (k, _) = line.split_once('=').unwrap();
            let key = k.trim().to_string();
            if out.keys.contains_key(&key) {
                out.duplicates.push(key.clone());
            }
            out.keys.entry(key.clone()).or_default();
            current = Some(key);
        }
        if let Some(key) = &current {
            if let Some(args) = out.keys.get_mut(key) {
                for ph in placeholders(line) {
                    args.insert(ph);
                }
            }
        }
    }
    out
}

/// 提取一行里全部 `$identifier` 占位符。
fn placeholders(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > i + 1 {
                out.push(line[i + 1..j].to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn i18n_check() -> Result<(), String> {
    let locales_dir = workspace_root().join("locales");
    let mut errors: Vec<String> = Vec::new();
    let mut checked_files = 0usize;
    let mut checked_keys = 0usize;
    // 全部 FTL key 的并集（跨 4 个文件 × 6 种语言），供代码引用扫描用。
    let mut all_ftl_keys: BTreeSet<String> = BTreeSet::new();

    for file in FTL_FILES {
        // 各语言的解析结果。
        let mut per_locale: BTreeMap<String, FtlFile> = BTreeMap::new();
        for locale in LOCALES {
            let path = locales_dir.join(locale).join(file);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    errors.push(format!("{}：无法读取 {}：{e}", locale, path.display()));
                    continue;
                }
            };
            // 基础语法：花括号必须成对（Fluent 值里的 `{ ... }`）。
            let opens = text.matches('{').count();
            let closes = text.matches('}').count();
            if opens != closes {
                errors.push(format!(
                    "{locale}/{file}：花括号不成对（`{{` {opens} 个，`}}` {closes} 个）"
                ));
            }
            per_locale.insert(locale.to_string(), parse_ftl(&text));
        }
        let union: BTreeSet<String> = per_locale
            .values()
            .flat_map(|f| f.keys.keys().cloned())
            .collect();
        checked_keys += union.len();
        checked_files += 1;
        all_ftl_keys.extend(union.iter().cloned());
        for locale in LOCALES {
            let Some(parsed) = per_locale.get(*locale) else {
                continue;
            };
            for dup in &parsed.duplicates {
                errors.push(format!("{locale}/{file}：重复 key `{dup}`"));
            }
            for key in union.difference(&parsed.keys.keys().cloned().collect()) {
                errors.push(format!("{locale}/{file}：缺 key `{key}`"));
            }
            for key in parsed.keys.keys() {
                if !union.contains(key) {
                    errors.push(format!("{locale}/{file}：多余 key `{key}`"));
                }
            }
            // 占位符一致性：同一 key 在各语言里的 `$var` 集合必须相同。
            for key in &union {
                let Some(args) = parsed.keys.get(key) else {
                    continue;
                };
                let reference = per_locale.get("zh-Hant-TW").and_then(|f| f.keys.get(key));
                if let Some(reference) = reference {
                    if args != reference {
                        errors.push(format!(
                            "{locale}/{file}：key `{key}` 的占位符与 zh-Hant-TW 不一致（本语言 {args:?}，zh-Hant-TW {reference:?}）"
                        ));
                    }
                }
            }
        }
    }

    // M11：扫描代码里的 t_or( / t_args_or( 引用，与 FTL key 集比对。
    // 只做 FTL 间一致性校验时，49 个缺 key 也能全绿——这一步堵住该漏网。
    let (referenced_count, mut scan_errors) = scan_code_references(&all_ftl_keys);
    errors.append(&mut scan_errors);

    if errors.is_empty() {
        println!(
            "i18n-check 通过：{checked_files} 个文件 × {} 种语言，共 {checked_keys} 个 key，缺 key / 多余 key / 占位符不一致均为 0；代码引用扫描：{referenced_count} 个不同 key 全部在 FTL 中",
            LOCALES.len()
        );
        Ok(())
    } else {
        for e in &errors {
            eprintln!("error: {e}");
        }
        Err(format!("i18n-check 失败：{} 个问题", errors.len()))
    }
}

/// 递归收集 `dir` 下全部 `.rs` 文件。
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// 在一段 Rust 源码里找出全部 `t_or("key", …)` / `t_args_or("key", …)`
/// 调用，返回 `(key, key 所在行号)`。key 与调用可能跨行（如
/// `t_args_or(\n    "portal.quota-used", …)`），故按整段文本扫描而非逐行。
fn scan_t_or_calls(text: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // `t_args_or(` 含 `_or(` 但不含 `t_or(`，先判长模式。
        // 用字节比较（不用 `text[i..]` 切片），避免 `i` 落在多字节字符
        // 中间时 panic。
        let pat_len = if bytes[i..].starts_with(b"t_args_or(") {
            10
        } else if bytes[i..].starts_with(b"t_or(") {
            5
        } else {
            i += 1;
            continue;
        };
        // 调用名前必须是分隔符（排除 `foo_t_or(` 之类的标识符内命中）。
        if i > 0 {
            let prev = bytes[i - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                i += pat_len;
                continue;
            }
        }
        let mut j = i + pat_len;
        while j < bytes.len() && (bytes[j] as char).is_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'"' {
            // 不是字符串首参（如 `pub fn t_or(key: &str, …)` 的定义处），跳过。
            i += pat_len;
            continue;
        }
        let start = j + 1;
        let mut k = start;
        while k < bytes.len() && bytes[k] != b'"' {
            k += 1;
        }
        if k < bytes.len() {
            let key = text[start..k].to_string();
            let line_no = text[..start].matches('\n').count() + 1;
            out.push((key, line_no));
        }
        i = k.max(j + 1);
    }
    out
}

/// 扫描 `crates/` 下全部 `.rs` 的 `t_or(`/`t_args_or(` 引用，与 FTL key 集
/// 比对。返回（引用的不同 key 数，缺 key 错误列表）。`t()` 的点转横线规则
/// 与 `I18n::t` 一致：全部 `.` 转 `-`（如 `portal.quota-used`）。
fn scan_code_references(ftl_keys: &BTreeSet<String>) -> (usize, Vec<String>) {
    let root = workspace_root();
    let mut files = Vec::new();
    collect_rs_files(&root.join("crates"), &mut files);
    let mut referenced: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (key, line_no) in scan_t_or_calls(&text) {
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            referenced
                .entry(key)
                .or_default()
                .push(format!("{rel}:{line_no}"));
        }
    }
    let mut errors = Vec::new();
    for (dotted, locations) in &referenced {
        let ftl_key = dotted.replace('.', "-");
        if !ftl_keys.contains(&ftl_key) {
            errors.push(format!(
                "代码引用了 FTL 中缺失的 key `{dotted}`（FTL 形式 `{ftl_key}`），引用处：{}",
                locations.join("、")
            ));
        }
    }
    (referenced.len(), errors)
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    println!("$ {cmd} {}", args.join(" "));
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| format!("无法执行 {cmd}：{e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} {} 失败（退出码 {status}）", args.join(" ")))
    }
}

fn check_all() -> Result<(), String> {
    run("cargo", &["fmt", "--check"])?;
    run(
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run("cargo", &["test", "--workspace"])?;
    println!("check-all 通过：fmt / clippy / test 全部通过");
    Ok(())
}

fn usage() -> ! {
    eprintln!("用法：cargo xtask <i18n-check|check-all>");
    eprintln!("  i18n-check  检查 locales/ 下 5 种语言 + en-XA 的 .ftl key 齐全与占位符一致");
    eprintln!("  check-all    依次跑 cargo fmt --check、clippy、test");
    std::process::exit(2);
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let cmd = match args.next() {
        Some(c) => c,
        None => usage(),
    };
    let result = match cmd.as_str() {
        "i18n-check" => i18n_check(),
        "check-all" => check_all(),
        _ => usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
