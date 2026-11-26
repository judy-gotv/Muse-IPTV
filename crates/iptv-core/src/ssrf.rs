//! SSRF 校验（开发文档 §13）：拉取目标只允许公网 http/https 地址。
//!
//! 由 `iptv-worker` 与 `iptv-api`（`proxy.rs`）共用，校验逻辑只此一份，
//! 避免两处各自手写后漂移。注意这是请求前的尽力校验，解析与建连之间
//! 存在 TOCTOU 窗口（DNS 重绑定），不能当作绝对屏障；重定向目标须由
//! 调用方逐跳重调 `check_url_allowed`（M17①），不要只校验首跳。
// 作者: Passwd Word

use std::net::IpAddr;

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
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip_is_blocked(ip);
    }
    false
}

/// 把 host 解析成全部 IP：字面 IP 直接返回单元素；域名走异步 DNS。
/// 解析失败返回 Err（调用方应按“拒绝”处理，不要按“放行”处理）。
pub async fn resolve_host_ips(host: &str, port: u16) -> anyhow::Result<Vec<IpAddr>> {
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
    if host.parse::<IpAddr>().is_err() {
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
    }
}
