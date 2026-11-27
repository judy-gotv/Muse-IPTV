//! SSRF 校验（开发文档 §13）：拉取目标只允许公网 http/https 地址。
//!
//! 由 `iptv-worker` 与 `iptv-api`（`proxy.rs`）共用，校验逻辑只此一份，
//! 避免两处各自手写后漂移。N2：仅靠请求前校验不够——解析与建连之间
//! 存在 TOCTOU 窗口（DNS 重绑定）；调用方须走 `resolve_checked_ip` /
//! `resolve_segment_ip` 解析一次并拿到校验过的 IP，再用 `pin_target`
//! 的返回值经 `ClientBuilder::resolve` 把连接固定到该 IP（reqwest 的
//! 覆盖只替换 DNS 解析，TLS SNI/Host 仍用原主机名）。重定向目标须由
//! 调用方逐跳重新解析+校验+固定（M17①），不要只校验首跳。
// 作者: Passwd Word

use std::net::{IpAddr, SocketAddr};

/// 该 IP 是否禁止作为拉取目标：私网、回环、链路本地、未指定、组播、
// 广播、文档/基准测试/保留段、CGNAT 共享段；v6 另加唯一本地与单播链路本地。
pub fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                // 以下三段在 1.89 仍不稳定（`ip` 特性），手写段判断：
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 基准测试 198.18.0.0/15
                || o[0] >= 240 // 保留段 240.0.0.0/4
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 共享段 100.64.0.0/10
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
        }
    }
}

pub fn is_private_host(host: &str) -> bool {
    // 域名先放行（解析后校验在 reqwest 层无法拦截，故只拦字面 IP）。
    // `Url::host_str()` 对 IPv6 字面量返回带括号的形式（`[::1]`），
    // 先去括号再解析，否则字面 IPv6 永远判不成 IP。
    if let Ok(ip) = strip_ip_brackets(host).parse::<IpAddr>() {
        return ip_is_blocked(ip);
    }
    false
}

/// `Url::host_str()` 对 IPv6 字面量返回带括号的形式（`[::1]`），
/// `IpAddr::from_str` 与 `lookup_host` 都不接受括号；去括号后再用。
/// 域名与 IPv4 不受影响。
fn strip_ip_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// 把 host 解析成全部 IP：字面 IP 直接返回单元素；域名走异步 DNS。
/// 解析失败返回 Err（调用方应按“拒绝”处理，不要按“放行”处理）。
pub async fn resolve_host_ips(host: &str, port: u16) -> anyhow::Result<Vec<IpAddr>> {
    let host = strip_ip_brackets(host);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    let addrs = tokio::net::lookup_host((host, port)).await?;
    Ok(addrs.map(|a| a.ip()).collect())
}

/// 两组 IP 是否有交集（M17③：同一主机名两次解析的子集可能因轮询顺序
/// 不同而不同，故只在“两侧都只解析出一个 IP”时才要求严格相等，
/// 见 `proxy.rs`；多 IP 场景用 `all_ips_public` 兜底）。
pub fn ips_overlap(a: &[IpAddr], b: &[IpAddr]) -> bool {
    a.iter().any(|ip| b.contains(ip))
}

/// 该组 IP 是否全部不在禁段（任一命中即 false）。
pub fn all_ips_public(ips: &[IpAddr]) -> bool {
    !ips.is_empty() && ips.iter().all(|ip| !ip_is_blocked(*ip))
}

/// 仅供测试的放行开关：`IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS=1` 时跳过私网
/// IP 段校验。**生产绝不能设置**：沙箱 DNS 把域名劫持到 198.18/15，
/// 且沙箱仅有的本地地址全在禁段，不开开关就无法在沙箱内做正向播放
/// 冒烟；正向播放须在真实服务器（正常 DNS）上再验一次。
/// 注意：开关读的是进程环境变量，单元测试里开关它会与同进程并行的
/// 阻塞断言竞态，故不在此写开关的单元测试，改为冒烟脚本整进程设置后验证。
pub fn test_allow_private_upstreams() -> bool {
    std::env::var("IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS").is_ok_and(|v| v == "1")
}

/// SSRF 校验（M3）：仅 http/https；把 host 解析成 IP 后逐一校验，
/// 任一解析结果落在禁段即拒绝。
///
/// 开关见 `test_allow_private_upstreams`（scheme 校验保留，只放行 IP 段）。
pub async fn check_url_allowed(url: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(url)?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        anyhow::bail!("only http/https urls allowed");
    }
    if test_allow_private_upstreams() {
        return Ok(());
    }
    let Some(host) = parsed.host_str() else {
        anyhow::bail!("url has no host");
    };
    if is_private_host(host) {
        anyhow::bail!("url host is private");
    }
    if strip_ip_brackets(host).parse::<IpAddr>().is_err() {
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs = tokio::net::lookup_host((host, port)).await?;
        for addr in addrs {
            if ip_is_blocked(addr.ip()) {
                anyhow::bail!("url resolves to private address {addr}");
            }
        }
    }
    Ok(())
}

/// 解析一次并校验，返回校验通过的那个 IP（N2，`check_url_allowed` 的
/// “解析+返回”版）：仅 http/https；域名解析出的全部 IP 都须不在禁段
/// （任一命中即拒绝；空解析结果也拒绝——无 IP 可固定）；字面私网 IP
/// 同样拒绝。开关 `IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS=1` 时只跳过 IP
/// 段校验（scheme 校验保留），仍须解析成功才能固定。
/// 返回首个 IP：多 IP 时只固定其中一个已校验通过的，调用方须把返回值
/// 经 `pin_target` 传给 `ClientBuilder::resolve`，不要再让客户端二
/// 次解析同一主机名。
pub async fn resolve_checked_ip(url: &str) -> anyhow::Result<IpAddr> {
    let parsed = url::Url::parse(url)?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        anyhow::bail!("only http/https urls allowed");
    }
    let Some(host) = parsed.host_str() else {
        anyhow::bail!("url has no host");
    };
    if is_private_host(host) && !test_allow_private_upstreams() {
        anyhow::bail!("url host is private");
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    let ips = resolve_host_ips(host, port).await?;
    if ips.is_empty() {
        anyhow::bail!("url resolves to no address");
    }
    if !test_allow_private_upstreams() {
        if let Some(bad) = ips.iter().find(|ip| ip_is_blocked(**ip)) {
            anyhow::bail!("url resolves to private address {bad}");
        }
    }
    Ok(ips[0])
}

/// 分片目标解析+校验并返回固定用的 IP（N2；M17③ 的规则收进 core 一份，
/// `proxy.rs::check_segment_target` 与 `fetch_segment_checked` 共用，
/// 避免两处手写后漂移）：字符串层 host/scheme/端口须与频道主地址一致；
/// 目标解析出的任一 IP 落禁段即拒绝（解析失败、空结果也按拒绝处理）；
/// 两侧都只解析出一个 IP 时要求严格相等。多 IP 场景（CDN 轮询）只
/// 执行“全部公网”规则，避免误杀正常轮询。开关打开时跳过 IP 层两条
/// 规则（字符串层仍保留），仍须解析成功才能固定。
pub async fn resolve_segment_ip(upstream: &url::Url, target: &url::Url) -> anyhow::Result<IpAddr> {
    if target.host_str() != upstream.host_str()
        || target.scheme() != upstream.scheme()
        || target.port_or_known_default() != upstream.port_or_known_default()
    {
        anyhow::bail!("segment host not allowed");
    }
    let Some(target_host) = target.host_str() else {
        anyhow::bail!("segment host not allowed");
    };
    let Some(upstream_host) = upstream.host_str() else {
        anyhow::bail!("segment host not allowed");
    };
    let target_port = target.port_or_known_default().unwrap_or(443);
    let target_ips = resolve_host_ips(target_host, target_port).await?;
    if target_ips.is_empty() {
        anyhow::bail!("segment host resolves to no address");
    }
    if test_allow_private_upstreams() {
        return Ok(target_ips[0]);
    }
    if !all_ips_public(&target_ips) {
        anyhow::bail!("segment host resolves to private address");
    }
    let upstream_port = upstream.port_or_known_default().unwrap_or(443);
    let upstream_ips = resolve_host_ips(upstream_host, upstream_port).await?;
    if target_ips.len() == 1 && upstream_ips.len() == 1 && !ips_overlap(&target_ips, &upstream_ips)
    {
        anyhow::bail!("segment host ip differs from upstream ip");
    }
    Ok(target_ips[0])
}

/// 固定连接用的 DNS 覆盖（N2）：域名 host 返回 `(host, ip:port)`，
/// 供调用方 `ClientBuilder::resolve(&host, addr)`；字面 IP 返回 `None`
/// （无 DNS 可重绑定，reqwest 直连该 IP 即可）。reqwest 的覆盖只替换
/// DNS 解析：TLS SNI 与 Host 头仍用原主机名，证书校验不受影响。
/// host 统一小写：`url` 解析已把域名小写化，这里再防御一次，避免
/// 覆盖键大小写不一致导致回落到真实 DNS。
pub fn pin_target(url: &url::Url, ip: IpAddr) -> Option<(String, SocketAddr)> {
    let host = strip_ip_brackets(url.host_str()?).to_ascii_lowercase();
    if host.parse::<IpAddr>().is_ok() {
        return None;
    }
    let port = url.port_or_known_default().unwrap_or(443);
    Some((host, SocketAddr::new(ip, port)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn private_host_detection() {
        assert!(is_private_host("127.0.0.1"));
        assert!(is_private_host("10.0.0.5"));
        assert!(is_private_host("192.168.1.1"));
        assert!(!is_private_host("8.8.8.8"));
        assert!(!is_private_host("example.com"));
        // `Url::host_str()` 对 IPv6 字面量带括号，去括号后才能判成 IP。
        assert!(is_private_host("[::1]"));
        assert!(!is_private_host("[2001:4860:4860::8888]"));
    }

    #[test]
    fn blocked_ranges() {
        // 链路本地 / 元数据服务 / CGNAT / 基准测试 / 保留段一律禁。
        for ip in [
            "169.254.169.254",
            "100.64.0.1",
            "198.18.0.1",
            "240.0.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "192.0.2.1",
        ] {
            assert!(ip_is_blocked(ip.parse().unwrap()), "{ip} should be blocked");
        }
        assert!(!ip_is_blocked("8.8.8.8".parse().unwrap()));
        assert!(!ip_is_blocked("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn ips_overlap_and_public() {
        let a: Vec<IpAddr> = vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))];
        let b: Vec<IpAddr> = vec![
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
        ];
        assert!(ips_overlap(&a, &b));
        assert!(!ips_overlap(&a, &[IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9))]));
        assert!(all_ips_public(&b));
        assert!(!all_ips_public(&[IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))]));
        assert!(!all_ips_public(&[]));
    }

    #[tokio::test]
    async fn literal_ip_url_check() {
        assert!(check_url_allowed("http://127.0.0.1/x.m3u").await.is_err());
        assert!(
            check_url_allowed("http://169.254.169.254/latest/meta-data/")
                .await
                .is_err()
        );
        assert!(check_url_allowed("ftp://example.com/x").await.is_err());
        // 正向用字面公网 IP：不依赖外部 DNS（沙箱 DNS 会把域名劫持到
        // 198.18/15，域名正向用例在沙箱里无法断言）。
        assert!(check_url_allowed("http://8.8.8.8/x").await.is_ok());
    }

    #[tokio::test]
    async fn resolve_literal_ip() {
        let ips = resolve_host_ips("127.0.0.1", 80).await.unwrap();
        assert_eq!(ips.len(), 1);
        assert!(ips_overlap(
            &ips,
            &[IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))]
        ));
        // 带括号的 IPv6 字面量（`Url::host_str()` 的形式）同样直接返回。
        let ips = resolve_host_ips("[2001:4860:4860::8888]", 80)
            .await
            .unwrap();
        assert_eq!(ips.len(), 1);
        assert_eq!(ips[0], "2001:4860:4860::8888".parse::<IpAddr>().unwrap());
    }

    #[tokio::test]
    async fn resolve_checked_ip_literal() {
        // 字面公网 IP：不依赖外部 DNS，直接返回该 IP。
        assert_eq!(
            resolve_checked_ip("http://8.8.8.8/x").await.unwrap(),
            "8.8.8.8".parse::<IpAddr>().unwrap()
        );
        // 字面私网 IP 与非 http(s) 照样拒绝（开关未开）。
        assert!(resolve_checked_ip("http://127.0.0.1/x").await.is_err());
        assert!(resolve_checked_ip("http://10.0.0.5/x").await.is_err());
        assert!(resolve_checked_ip("ftp://8.8.8.8/x").await.is_err());
        // IPv6 字面地址同样走直连路径。
        assert_eq!(
            resolve_checked_ip("http://[2001:4860:4860::8888]/x")
                .await
                .unwrap(),
            "2001:4860:4860::8888".parse::<IpAddr>().unwrap()
        );
        assert!(resolve_checked_ip("http://[::1]/x").await.is_err());
    }

    #[tokio::test]
    async fn resolve_segment_ip_literal() {
        let upstream = url::Url::parse("http://8.8.8.8:8080/live.m3u8").unwrap();
        let same = url::Url::parse("http://8.8.8.8:8080/seg0.ts").unwrap();
        assert_eq!(
            resolve_segment_ip(&upstream, &same).await.unwrap(),
            "8.8.8.8".parse::<IpAddr>().unwrap()
        );
        // 单 IP 两侧不等即拒绝（M17③ 规则）。
        let other = url::Url::parse("http://8.8.8.8:8080/seg0.ts").unwrap();
        let upstream2 = url::Url::parse("http://1.1.1.1:8080/live.m3u8").unwrap();
        assert!(resolve_segment_ip(&upstream2, &other).await.is_err());
        // scheme / 端口 / host 任一不一致即拒绝。
        let https = url::Url::parse("https://8.8.8.8:8080/seg0.ts").unwrap();
        assert!(resolve_segment_ip(&upstream, &https).await.is_err());
        let port = url::Url::parse("http://8.8.8.8:9090/seg0.ts").unwrap();
        assert!(resolve_segment_ip(&upstream, &port).await.is_err());
        // 目标解析到私网 IP 即拒绝。
        let upstream3 = url::Url::parse("http://127.0.0.1:8080/live.m3u8").unwrap();
        let target3 = url::Url::parse("http://127.0.0.1:8080/seg0.ts").unwrap();
        assert!(resolve_segment_ip(&upstream3, &target3).await.is_err());
    }

    #[test]
    fn pin_target_rules() {
        // 域名返回 (host, ip:port)，端口取 URL 端口。
        let url = url::Url::parse("http://example.com:8080/x").unwrap();
        assert_eq!(
            pin_target(&url, "93.184.216.34".parse().unwrap()),
            Some((
                "example.com".to_string(),
                "93.184.216.34:8080".parse().unwrap()
            ))
        );
        // 无显式端口时取 scheme 默认端口。
        let url = url::Url::parse("https://example.com/x").unwrap();
        assert_eq!(
            pin_target(&url, "93.184.216.34".parse().unwrap()),
            Some((
                "example.com".to_string(),
                "93.184.216.34:443".parse().unwrap()
            ))
        );
        // 字面 IP（v4/v6）不固定：无 DNS 可重绑定。
        let url = url::Url::parse("http://8.8.8.8/x").unwrap();
        assert_eq!(pin_target(&url, "8.8.8.8".parse().unwrap()), None);
        let url = url::Url::parse("http://[2001:4860:4860::8888]/x").unwrap();
        assert_eq!(
            pin_target(&url, "2001:4860:4860::8888".parse().unwrap()),
            None
        );
        // IPv6 固定地址正常构造。
        let url = url::Url::parse("http://example.com/x").unwrap();
        assert_eq!(
            pin_target(&url, "2001:4860:4860::8888".parse().unwrap()),
            Some((
                "example.com".to_string(),
                "[2001:4860:4860::8888]:80".parse().unwrap()
            ))
        );
    }
}
