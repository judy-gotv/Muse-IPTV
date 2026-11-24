// Muse-IPTV Tauri 2 壳入口（开发文档 §2 跨端壳）。
// 内容仍是同一份 Leptos WASM 前端（portal），壳只提供原生窗口与标题栏。
// 构建：cd deploy/tauri-shell && cargo tauri build（需 Tauri CLI 与系统依赖）。
// 作者: Passwd Word
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
