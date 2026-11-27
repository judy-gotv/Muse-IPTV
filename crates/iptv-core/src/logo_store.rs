//! 台标转存 S3 兼容对象存储（开发文档 §7.2，m9）。
//!
//! - 配置走环境变量（`S3_ENDPOINT` / `S3_BUCKET` / `S3_REGION` /
//!   `S3_ACCESS_KEY` / `S3_SECRET_KEY` / `S3_PUBLIC_BASE_URL` 可选）：四项
//!   必填缺一即视为未配置，调用方保持上游 `logo_url` 直链（行为不变）。
//! - 下载走与订阅/EPG 拉取同款 SSRF 校验（N2：`resolve_checked_ip`
//!   解析一次并校验，再用 `pin_target` + `ClientBuilder::resolve`
//!   把连接固定到校验过的 IP，不再让 reqwest 二次解析，逐跳重验，
//!   最多 3 跳），不另开不校验的下载口；单张台标上限 5 MB。
//! - 上传用手写 SigV4 签名做 S3 PUT（path-style），只依赖工作区已有的
//!   `reqwest` / `sha2` / `hmac`，不引入 aws-sdk-s3 这类重依赖。
//! - 对象键 `logos/<sha256(内容)>.<ext>`，内容相同即同一对象；入库的
//!   是转存后 URL，`S3_PUBLIC_BASE_URL` 未配时拼
//!   `{endpoint}/{bucket}/{key}`。
//! - 转存失败不抛给调用方主流程：`mirror_logo_if_configured` 只在
//!   配置齐全且下载+上传都成功时返回 `Some(新 URL)`，其余返回 `Ok(None)`
//!   并记 `tracing::warn`，调用方保留直链。
//! - 缩略图（§7.2 后半句）本轮未做：转存的是原图，`logo_url` 仍指向
//!   全尺寸图。
// 作者: Passwd Word

use crate::ssrf::{pin_target, resolve_checked_ip};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// 单张台标下载上限：5 MB（台标是小图；超限按失败处理、回落直链）。
pub const LOGO_MAX_BYTES: usize = 5 * 1024 * 1024;

/// 每轮同步最多转存的台标数（大源首次同步不把 worker 拖垮；超出的
/// 频道保留直链，下次内容变化时再转存）。
pub const LOGO_MIRROR_PER_SYNC: usize = 200;

/// S3 兼容对象存储配置（全部来自环境变量，见 `from_env`）。
#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    /// 可选：公开访问地址前缀（如 CDN 域名）；未配则拼
    /// `{endpoint}/{bucket}/{key}`。
    pub public_base_url: Option<String>,
}

impl S3Config {
    /// 从环境变量读配置；`S3_ENDPOINT` / `S3_BUCKET` / `S3_ACCESS_KEY` /
    /// `S3_SECRET_KEY` 四项缺一或为空即返回 `None`（未配置）。
    /// `S3_REGION` 缺省 `us-east-1`；`S3_PUBLIC_BASE_URL` 可选。
    /// 凭证只进内存，绝不写进代码或文档示例值。
    pub fn from_env() -> Option<Self> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// `from_env` 的可测试内核：变量来源由调用方注入，单测不用改进程环境。
    pub fn from_vars(vars: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let non_empty = |k: &str| {
            vars(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Some(Self {
            endpoint: non_empty("S3_ENDPOINT")?,
            bucket: non_empty("S3_BUCKET")?,
            region: non_empty("S3_REGION").unwrap_or_else(|| "us-east-1".to_string()),
            access_key: non_empty("S3_ACCESS_KEY")?,
            secret_key: non_empty("S3_SECRET_KEY")?,
            public_base_url: non_empty("S3_PUBLIC_BASE_URL"),
        })
    }

    /// PUT 目标地址（path-style）：`{endpoint}/{bucket}/{key}`。
    pub fn put_url(&self, key: &str) -> String {
        format!(
            "{}/{}/{}",
            self.endpoint.trim_end_matches('/'),
            self.bucket,
            key
        )
    }

    /// 入库用的公开地址：配了 `S3_PUBLIC_BASE_URL` 就拼它，否则同 PUT 地址。
    pub fn public_url(&self, key: &str) -> String {
        match &self.public_base_url {
            Some(base) => format!("{}/{}", base.trim_end_matches('/'), key),
            None => self.put_url(key),
        }
    }
}

/// 台标对象键：`logos/<sha256(图片字节)>.<ext>`。
/// 扩展名先看 `Content-Type`，再看源 URL 路径后缀（白名单内才认），
/// 都认不出返回 `None`（调用方回落直链，不猜格式）。
pub fn object_key_for(
    bytes: &[u8],
    content_type: Option<&str>,
    source_url: &str,
) -> Option<String> {
    let ext = ext_from_content_type(content_type)
        .or_else(|| ext_from_url(source_url))
        .filter(|e| !e.is_empty())?;
    Some(format!("logos/{}.{}", sha256_hex(bytes), ext))
}

fn ext_from_content_type(content_type: Option<&str>) -> Option<&'static str> {
    let ct = content_type?.split(';').next()?.trim().to_ascii_lowercase();
    Some(match ct.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/x-icon" | "image/vnd.microsoft.icon" => "ico",
        _ => return None,
    })
}

fn ext_from_url(source_url: &str) -> Option<&'static str> {
    let path = source_url
        .rsplit(['?', '#'])
        .next_back()
        .unwrap_or(source_url);
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    if path.contains('.') && !ext.contains('/') {
        match ext.as_str() {
            "png" => Some("png"),
            "jpg" | "jpeg" => Some("jpg"),
            "gif" => Some("gif"),
            "webp" => Some("webp"),
            "svg" => Some("svg"),
            "ico" => Some("ico"),
            _ => None,
        }
    } else {
        None
    }
}

/// 转存一张台标：配置缺失、地址为空、下载失败、非图片、上传失败
/// 任一情况都返回 `Ok(None)`（调用方保留上游直链）；只有下载 +
/// 上传都成功才返回 `Ok(Some(转存后公开 URL))`。
/// 下载走 N2 固定 IP 版逐跳 SSRF 校验（与 worker / proxy 的
/// `fetch_checked` 同款规则：仅 http/https、解析一次校验后固定
/// 连接、最多 3 跳）。
pub async fn mirror_logo_if_configured(
    cfg: Option<&S3Config>,
    logo_url: &str,
) -> anyhow::Result<Option<String>> {
    let Some(cfg) = cfg else {
        return Ok(None);
    };
    let logo_url = logo_url.trim();
    if logo_url.is_empty() {
        return Ok(None);
    }
    match mirror_logo(cfg, logo_url).await {
        Ok(url) => Ok(Some(url)),
        Err(e) => {
            tracing::warn!("logo mirror failed for {logo_url}: {e:#}");
            Ok(None)
        }
    }
}

async fn mirror_logo(cfg: &S3Config, logo_url: &str) -> anyhow::Result<String> {
    let (bytes, content_type) = fetch_logo_checked(logo_url).await?;
    upload_logo(cfg, &bytes, content_type.as_deref(), logo_url).await
}

/// 上传已下载好的台标字节：按内容定对象键、SigV4 PUT、返回公开地址。
/// 下载路径由调用方定（worker 走自身的 `fetch_checked` 固定 IP 版；
/// API 走这里的 `mirror_logo_if_configured`，同样逐跳固定 IP），上传目标是
/// 运营方配置的 S3 端点，不走 SSRF 校验。
pub async fn upload_logo(
    cfg: &S3Config,
    bytes: &[u8],
    content_type: Option<&str>,
    source_url: &str,
) -> anyhow::Result<String> {
    let key = object_key_for(bytes, content_type, source_url)
        .ok_or_else(|| anyhow::anyhow!("unrecognized logo content type"))?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    put_object(&client, cfg, &key, bytes, content_type).await?;
    Ok(cfg.public_url(&key))
}

/// 带逐跳 SSRF 校验的台标下载（N2）：每跳先 `resolve_checked_ip`
/// 解析一次并校验，再用 `pin_target` + `ClientBuilder::resolve` 把
/// 连接固定到该 IP，不再让 reqwest 二次解析同一主机名，DNS 重绑定
/// 无窗口；3xx 带 Location 时解析相对地址后继续，最多跟 3 跳；
/// 响应体流式读，超 `LOGO_MAX_BYTES` 立即断流报错。
async fn fetch_logo_checked(url: &str) -> anyhow::Result<(Vec<u8>, Option<String>)> {
    let mut current = url.to_string();
    for _ in 0..=3 {
        let ip = resolve_checked_ip(&current).await?;
        let parsed = reqwest::Url::parse(&current)?;
        let client = pinned_client(&parsed, ip);
        let mut resp = client.get(&current).send().await?;
        if resp.status().is_redirection() {
            let Some(loc) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
            else {
                anyhow::bail!("logo redirect without location: {current}");
            };
            let base = reqwest::Url::parse(&current)?;
            current = base.join(&loc).map(|u| u.to_string())?;
            continue;
        }
        if !resp.status().is_success() {
            anyhow::bail!("logo fetch failed: {}", resp.status());
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let mut buf = Vec::new();
        loop {
            let Some(chunk) = resp.chunk().await? else {
                break;
            };
            if buf.len() + chunk.len() > LOGO_MAX_BYTES {
                anyhow::bail!("logo too large (max {LOGO_MAX_BYTES} bytes)");
            }
            buf.extend_from_slice(&chunk);
        }
        return Ok((buf, content_type));
    }
    anyhow::bail!("too many redirects");
}

/// N2：按校验过的 IP 建客户端（`ClientBuilder::resolve` 覆盖本次 DNS
/// 解析；字面 IP 主机无需固定，见 `pin_target`）。重定向目标 host
/// 不同故每跳新建；超时 15s 与 API 原调用方一致。
fn pinned_client(url: &reqwest::Url, ip: std::net::IpAddr) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("Muse-IPTV/{}", crate::VERSION))
        .redirect(reqwest::redirect::Policy::none());
    if let Some((host, addr)) = pin_target(url, ip) {
        builder = builder.resolve(&host, addr);
    }
    builder.build().expect("http client")
}

/// SigV4 签名 PUT 一个对象到 S3（path-style）。签名头：
/// `host` / `x-amz-content-sha256` / `x-amz-date`。
async fn put_object(
    client: &reqwest::Client,
    cfg: &S3Config,
    key: &str,
    bytes: &[u8],
    content_type: Option<&str>,
) -> anyhow::Result<()> {
    let url = cfg.put_url(key);
    let payload_hash = sha256_hex(bytes);
    let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let auth = authorization_header(cfg, "PUT", &url, &payload_hash, &amz_date, "s3")?;
    let mut req = client
        .put(&url)
        .header(reqwest::header::AUTHORIZATION, auth)
        .header("x-amz-date", amz_date.as_str())
        .header("x-amz-content-sha256", payload_hash)
        .body(bytes.to_vec());
    if let Some(ct) = content_type {
        req = req.header(reqwest::header::CONTENT_TYPE, ct);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "s3 put failed: {status} {}",
            body.chars().take(300).collect::<String>()
        );
    }
    Ok(())
}

// --- SigV4（纯函数，可单测） ---

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], msg: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(msg.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// 规范请求：`METHOD\nURI\nQUERY\nHEADERS\n\nSIGNED\nPAYLOAD_HASH`。
/// `headers` 为待签名头（名已小写、值已去空白），调用方须按名排好序。
/// URI 按段做 RFC3986 编码（`/` 保留分隔）；S3 不做路径归一化，
/// 故不压缩 `//`、不解析 `.` / `..`。
pub fn canonical_request(
    method: &str,
    uri: &str,
    query: &str,
    headers: &[(&str, &str)],
    payload_hash: &str,
) -> String {
    let encoded_uri = uri
        .split('/')
        .map(encode_uri_segment)
        .collect::<Vec<_>>()
        .join("/");
    let canonical_query = canonical_query_string(query);
    let mut canonical_headers = String::new();
    let mut signed = Vec::new();
    for (name, value) in headers {
        canonical_headers.push_str(&format!("{}:{}\n", name.trim(), value.trim()));
        signed.push(name.trim());
    }
    format!(
        "{method}\n{encoded_uri}\n{canonical_query}\n{canonical_headers}\n{}\n{payload_hash}",
        signed.join(";")
    )
}

fn encode_uri_segment(seg: &str) -> String {
    let mut out = String::new();
    for b in seg.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn canonical_query_string(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (encode_uri_segment(k), encode_uri_segment(v)),
            None => (encode_uri_segment(p), String::new()),
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// 待签字符串：`AWS4-HMAC-SHA256\nAMZ_DATE\nSCOPE\nSHA256(CANONICAL)`。
pub fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    )
}

fn signing_key(secret_key: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date_stamp);
    let k_region = hmac_sha256(&k_date, region);
    let k_service = hmac_sha256(&k_region, service);
    hmac_sha256(&k_service, "aws4_request")
}

/// 完整的 `Authorization` 头值。`url` 为请求全地址；`amz_date` 形如
/// `20150830T123600Z`；PUT 签名的头为 `host` /
/// `x-amz-content-sha256` / `x-amz-date`（三者都在请求里发送，
/// S3 要求日期头必须在签名头集合内）。
pub fn authorization_header(
    cfg: &S3Config,
    method: &str,
    url: &str,
    payload_hash: &str,
    amz_date: &str,
    service: &str,
) -> anyhow::Result<String> {
    authorization_header_with_headers(
        cfg,
        method,
        url,
        payload_hash,
        amz_date,
        service,
        &[
            ("x-amz-content-sha256", payload_hash),
            ("x-amz-date", amz_date),
        ],
    )
}

/// `authorization_header` 的可测试内核：`extra_headers` 为除 host 外
/// 待签名的头（名已小写），按名排序后与 host 一起签名；PUT 用
/// `x-amz-content-sha256` + `x-amz-date`，官方向量用 `x-amz-date`。
fn authorization_header_with_headers(
    cfg: &S3Config,
    method: &str,
    url: &str,
    payload_hash: &str,
    amz_date: &str,
    service: &str,
    extra_headers: &[(&str, &str)],
) -> anyhow::Result<String> {
    let parsed = url::Url::parse(url)?;
    let date_stamp = amz_date
        .get(..8)
        .ok_or_else(|| anyhow::anyhow!("bad amz date"))?;
    let scope = format!("{date_stamp}/{}/{service}/aws4_request", cfg.region);
    let host = match parsed.port() {
        Some(p) => format!("{}:{p}", parsed.host_str().unwrap_or("")),
        None => parsed.host_str().unwrap_or("").to_string(),
    };
    let mut headers: Vec<(&str, String)> = vec![("host", host)];
    for (name, value) in extra_headers {
        headers.push((name, value.to_string()));
    }
    headers.sort_by(|a, b| a.0.cmp(b.0));
    let header_refs: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
    let canonical = canonical_request(
        method,
        parsed.path(),
        parsed.query().unwrap_or(""),
        &header_refs,
        payload_hash,
    );
    let sts = string_to_sign(amz_date, &scope, &canonical);
    let key = signing_key(&cfg.secret_key, date_stamp, &cfg.region, service);
    let signature = hex::encode(hmac_sha256(&key, &sts));
    let signed_headers = header_refs
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(";");
    Ok(format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        cfg.access_key
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn example_cfg() -> S3Config {
        S3Config {
            endpoint: "https://s3.example.com".to_string(),
            bucket: "logos".to_string(),
            region: "us-east-1".to_string(),
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            public_base_url: None,
        }
    }

    /// AWS SigV4 官方测试套件 `get-vanilla-query` 向量（套件文件见
    /// aws-sig-v4-test-suite；Python 标准库 hmac/hashlib 独立复算过，
    /// canonical hash 与签名均与套件 `.creq` / `.sts` / `.authz` 一致）。
    #[test]
    fn sigv4_aws_official_get_vanilla_query_vector() {
        let cfg = S3Config {
            region: "us-east-1".to_string(),
            ..example_cfg()
        };
        let empty_hash = sha256_hex(b"");
        assert_eq!(
            empty_hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let canonical = canonical_request(
            "GET",
            "/",
            "",
            &[
                ("host", "example.amazonaws.com"),
                ("x-amz-date", "20150830T123600Z"),
            ],
            &empty_hash,
        );
        assert_eq!(
            canonical,
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(canonical.as_bytes()),
            "bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        let scope = "20150830/us-east-1/service/aws4_request";
        let sts = string_to_sign("20150830T123600Z", scope, &canonical);
        assert_eq!(
            sts,
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nbb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        let auth = authorization_header_with_headers(
            &cfg,
            "GET",
            "http://example.amazonaws.com/",
            &empty_hash,
            "20150830T123600Z",
            "service",
            &[("x-amz-date", "20150830T123600Z")],
        )
        .unwrap();
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn config_from_vars_requires_four_keys() {
        let mut vars = HashMap::new();
        vars.insert(
            "S3_ENDPOINT".to_string(),
            "https://s3.example.com".to_string(),
        );
        vars.insert("S3_BUCKET".to_string(), "logos".to_string());
        vars.insert("S3_ACCESS_KEY".to_string(), "AKID".to_string());
        // 缺 S3_SECRET_KEY → 未配置
        assert!(S3Config::from_vars(|k| vars.get(k).cloned()).is_none());
        vars.insert("S3_SECRET_KEY".to_string(), "SECRET".to_string());
        let cfg = S3Config::from_vars(|k| vars.get(k).cloned()).unwrap();
        assert_eq!(cfg.region, "us-east-1");
        assert_eq!(
            cfg.put_url("logos/abc.png"),
            "https://s3.example.com/logos/logos/abc.png"
        );
        assert_eq!(
            cfg.public_url("logos/abc.png"),
            "https://s3.example.com/logos/logos/abc.png"
        );
        vars.insert(
            "S3_PUBLIC_BASE_URL".to_string(),
            "https://cdn.example.com/".to_string(),
        );
        let cfg = S3Config::from_vars(|k| vars.get(k).cloned()).unwrap();
        assert_eq!(
            cfg.public_url("logos/abc.png"),
            "https://cdn.example.com/logos/abc.png"
        );
        // 空字符串视同未配
        vars.insert("S3_BUCKET".to_string(), "  ".to_string());
        assert!(S3Config::from_vars(|k| vars.get(k).cloned()).is_none());
    }

    #[test]
    fn object_key_is_content_addressed() {
        let bytes = b"\x89PNG fake logo bytes";
        let key =
            object_key_for(bytes, Some("image/png"), "https://cdn.example.com/a.png").unwrap();
        assert_eq!(key, format!("logos/{}.png", sha256_hex(bytes)));
        // 同内容、不同源 URL → 同一对象键（去重）；类型不同 → 后缀不同
        assert_eq!(
            object_key_for(bytes, Some("image/png"), "https://other.example/x.png").unwrap(),
            key
        );
        assert!(
            object_key_for(bytes, Some("image/jpeg"), "https://cdn.example.com/a")
                .unwrap()
                .ends_with(".jpg")
        );
        // Content-Type 未知时回落 URL 后缀；都认不出则 None（回落直链）
        assert!(
            object_key_for(bytes, Some("image/png"), "https://cdn.example.com/a")
                .unwrap()
                .ends_with(".png")
        );
        assert!(object_key_for(bytes, Some("text/html"), "https://cdn.example.com/a").is_none());
        assert!(object_key_for(bytes, None, "https://cdn.example.com/a.bmp").is_none());
    }

    /// 未配置存储时不碰网络、直接回落：`mirror_logo_if_configured`
    /// 在 cfg 为 None 时返回 Ok(None)，哪怕 URL 根本连不上。
    #[tokio::test]
    async fn unconfigured_storage_is_noop_without_network() {
        let out = mirror_logo_if_configured(None, "http://127.0.0.1:9/logo.png")
            .await
            .unwrap();
        assert_eq!(out, None);
        let out = mirror_logo_if_configured(Some(&example_cfg()), "  ")
            .await
            .unwrap();
        assert_eq!(out, None);
    }

    /// 校验失败（私网地址）也回落直链、不抛错：SSRF 校验在下载前。
    #[tokio::test]
    async fn blocked_logo_url_falls_back_to_direct_link() {
        let out = mirror_logo_if_configured(Some(&example_cfg()), "http://127.0.0.1/logo.png")
            .await
            .unwrap();
        assert_eq!(out, None);
    }

    /// 起一个本地 HTTP stub：每个连接读到请求头结束（`\r\n\r\n`）后
    /// 回固定响应并关闭连接。返回监听地址。
    async fn spawn_stub(ip: std::net::IpAddr, response: Vec<u8>) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let mut req = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                req.extend_from_slice(&buf[..n]);
                                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = sock.write_all(&response).await;
                });
            }
        });
        addr
    }

    /// 把域名追加进 `NO_PROXY`/`no_proxy` 的 RAII 守卫（只追加已有
    /// 值、不替换，drop 时恢复原值）。reqwest 在 `Client::build` 时
    /// 读代理环境变量，守卫须在建客户端之前生效。
    struct NoProxyGuard {
        prev_upper: Option<String>,
        prev_lower: Option<String>,
    }

    impl NoProxyGuard {
        fn append(domain: &str) -> Self {
            let prev_upper = std::env::var("NO_PROXY").ok();
            let prev_lower = std::env::var("no_proxy").ok();
            let add = |prev: &Option<String>| match prev {
                Some(v) if !v.is_empty() => format!("{v},{domain}"),
                _ => domain.to_string(),
            };
            std::env::set_var("NO_PROXY", add(&prev_upper));
            std::env::set_var("no_proxy", add(&prev_lower));
            Self {
                prev_upper,
                prev_lower,
            }
        }
    }

    impl Drop for NoProxyGuard {
        fn drop(&mut self) {
            match &self.prev_upper {
                Some(v) => std::env::set_var("NO_PROXY", v),
                None => std::env::remove_var("NO_PROXY"),
            }
            match &self.prev_lower {
                Some(v) => std::env::set_var("no_proxy", v),
                None => std::env::remove_var("no_proxy"),
            }
        }
    }

    /// N2 核心行为：固定后的客户端实际连向校验过的 IP。
    /// `pinned.test` 在 DNS 里不存在：未经固定的请求必然失败；固定到
    /// 回环 stub 的 IP 后，请求必须打到本地 stub 并返回其响应——证明
    /// `ClientBuilder::resolve` 的覆盖真的生效，而不是又走了一次 DNS。
    #[tokio::test]
    async fn pinned_client_connects_to_validated_ip() {
        let _no_proxy = NoProxyGuard::append("pinned.test");
        let loopback = std::net::IpAddr::from([127, 0, 0, 1]);
        let png: Vec<u8> = b"\x89PNG fake logo bytes".to_vec();
        let resp = [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n",
                png.len()
            )
            .into_bytes(),
            png.clone(),
        ]
        .concat();
        let addr = spawn_stub(loopback, resp).await;
        let url =
            reqwest::Url::parse(&format!("http://pinned.test:{}/logo.png", addr.port())).unwrap();
        let client = pinned_client(&url, addr.ip());
        let got = client.get(url).send().await.unwrap();
        assert_eq!(got.status(), 200);
        assert_eq!(got.bytes().await.unwrap().to_vec(), png);
    }
}
