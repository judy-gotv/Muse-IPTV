# hls.js（vendor，m16）
<!-- 作者: Passwd Word -->

- 文件：`hls.min.js`——hls.js **1.5.13**（UMD 构建，`window.Hls`）。
- 来源：`https://cdn.jsdelivr.net/npm/hls.js@1.5.13/dist/hls.min.js`
  （下载后离线 vendor，不走运行时 CDN 外链）。
- SHA-256：`e5753b10fc65eae35f21883e684d22c43ce66de968d35166792585b9b8a3f1a2`
- 服务：`crates/iptv-portal/src/main.rs` 的 `GET /vendor/hls.min.js`
  （`include_bytes!` 打进门户二进制，`Cache-Control: immutable`）。
- 升级：下载新版固定版本覆盖本文件，同步更新此处版本号与 SHA-256，
  并改 `crates/iptv-ui/src/lib.rs` 的 Player 注释。
