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
//! - 转存的是 128px 与 256px 两档缩略图（m9，§7.2 后半句），对象键
//!   `logos/thumbs/<sha256(缩略图字节)>.<ext>`，入库的 `logo_url` 指
//!   256px 档；原图不另存（只存缩略图，省存储、列表加载流量小）。
//!   缩略图生成/上传失败时回退到原图转存，不阻塞入库；WebP/ICO
//!   只做魔数校验、不解码（不引 webp 解码器），直接走原图转存。
//! - N9：转存白名单去掉 svg；下载字节先过魔数校验（PNG/JPEG/GIF/
//!   WebP/ICO），魔数不符（含 SVG/HTML）拒绝转存、保留直链。
//! - M-E：下载与 S3 上传两个客户端都 `.no_proxy()`，SSRF 固定 IP
//!   的路径不走环境代理。
// 作者: Passwd Word

use crate::ssrf::{pin_target, resolve_checked_ip};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// 单张台标下载上限：5 MB（台标是小图；超限按失败处理、回落直链）。
pub const LOGO_MAX_BYTES: usize = 5 * 1024 * 1024;

/// 每轮同步最多转存的台标数（大源首次同步不把 worker 拖垮；超出的
/// 频道保留直链，下次内容变化时再转存）。
pub const LOGO_MIRROR_PER_SYNC: usize = 200;

/// 缩略图两档的目标边长（m9）：等比缩放、最长边不超过该值。
pub const LOGO_THUMB_SIZES: [u32; 2] = [128, 256];

/// 入库的 `logo_url` 指哪一档缩略图（256px；128px 档同存，供小尺寸
/// 展示用）。
pub const LOGO_THUMB_URL_SIZE: u32 = 256;

/// 魔数识别出的图片格式（N9：只认这五种；SVG 是文本、无魔数，
/// 走到这里即被拒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedImage {
    Png,
    Jpeg,
    Gif,
    Webp,
    Ico,
}

impl DetectedImage {
    /// 对象键与 Content-Type 用的扩展名（以魔数为准，不信上游
    /// Content-Type / URL 后缀，避免类型冒充）。
    fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Ico => "ico",
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Ico => "image/x-icon",
        }
    }

    /// 能用 `image` crate 解码并重编码的格式（只开 png/jpeg/gif
    /// 三特性；WebP 解码需额外特性、ICO 无解码器，均不做缩略图）。
    fn thumbnail_format(self) -> Option<image::ImageFormat> {
        match self {
            Self::Png => Some(image::ImageFormat::Png),
            Self::Jpeg => Some(image::ImageFormat::Jpeg),
            Self::Gif => Some(image::ImageFormat::Gif),
            Self::Webp | Self::Ico => None,
        }
    }
}

/// 对下载字节做魔数校验（N9）：PNG 8 字节签名、JPEG `FF D8 FF`、
/// GIF `GIF87a`/`GIF89a`、WebP `RIFF....WEBP`、ICO `00 00 01 00`。
/// 不符返回 `None`（调用方拒绝转存、保留直链）；SVG/HTML/随机字节
/// 都落在这里。注意只看魔数，不看 Content-Type。
fn detect_image_format(bytes: &[u8]) -> Option<DetectedImage> {
    if bytes.len() >= 8 && bytes[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        Some(DetectedImage::Png)
    } else if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        Some(DetectedImage::Jpeg)
    } else if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        Some(DetectedImage::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(DetectedImage::Webp)
    } else if bytes.len() >= 4 && bytes[..4] == [0x00, 0x00, 0x01, 0x00] {
        Some(DetectedImage::Ico)
    } else {
        None
    }
}

/// 生成两档缩略图（m9）：解码一次、等比缩放到最长边不超过目标档
///（`thumbnail` 保持宽高比、不放大），按原格式重编码；返回
/// `(128px 字节, 256px 字节)`。任一步失败返回 `None`，调用方回退
/// 到原图转存（不阻塞入库）。动图 GIF 只取首帧（动画丢失，见注释）。
fn make_thumbnails(bytes: &[u8], format: image::ImageFormat) -> Option<(Vec<u8>, Vec<u8>)> {
    let img = image::load_from_memory_with_format(bytes, format).ok()?;
    let encode = |max_px: u32| -> Option<Vec<u8>> {
        let thumb = img.thumbnail(max_px, max_px);
        let mut buf = Vec::new();
        thumb
            .write_to(&mut std::io::Cursor::new(&mut buf), format)
            .ok()?;
        Some(buf)
    };
    Some((encode(LOGO_THUMB_SIZES[0])?, encode(LOGO_THUMB_SIZES[1])?))
}

/// 缩略图对象键：`logos/thumbs/<sha256(缩略图字节)>.<ext>`。
fn thumbnail_key_for(thumb_bytes: &[u8], ext: &str) -> String {
    format!("logos/thumbs/{}.{}", sha256_hex(thumb_bytes), ext)
}

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
        // N9：svg 不在白名单——SVG 是可执行 XML，原样转存到公开桶
        // 有存储型 XSS 面；上游是 SVG 时不转存、保留直链。
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
            // N9：svg 不在白名单（见 ext_from_content_type）。
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

/// 上传已下载好的台标字节：先过魔数校验（N9，魔数不符直接拒绝，
/// 调用方保留直链），再对 PNG/JPEG/GIF 生成 128/256 两档缩略图并
/// 上传（m9，`logo_url` 指 256px 档，原图不另存）；缩略图生成或
/// 上传失败时回退到原图转存（不阻塞入库）；WebP/ICO 不做缩略图，
/// 直接原图转存。下载路径由调用方定（worker 走自身的 `fetch_checked`
/// 固定 IP 版；API 走这里的 `mirror_logo_if_configured`，同样逐跳
/// 固定 IP），上传目标是运营方配置的 S3 端点，不走 SSRF 校验。
pub async fn upload_logo(
    cfg: &S3Config,
    bytes: &[u8],
    content_type: Option<&str>,
    source_url: &str,
) -> anyhow::Result<String> {
    // N9：魔数是唯一可信的类型依据；SVG/HTML/随机字节在这里被拒。
    let detected = detect_image_format(bytes)
        .ok_or_else(|| anyhow::anyhow!("unrecognized logo image magic"))?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        // M-E：S3 上传不走环境代理（运营方内网/直连端点不应被
        // http_proxy 劫持；与 pinned_client 同款处理）。
        .no_proxy()
        .build()?;
    // m9：可解码的格式先试缩略图；成功则直接返回 256px 档地址。
    if let Some(format) = detected.thumbnail_format() {
        match try_upload_thumbnails(&client, cfg, bytes, format, detected).await {
            Some(url) => return Ok(url),
            None => {
                tracing::warn!("logo thumbnail failed, falling back to original upload");
            }
        }
    }
    // 回退路径（缩略图失败，或 WebP/ICO 不做缩略图）：原图转存；
    // 对象键仍走 `object_key_for`（上游声明与魔数一致时保持旧键，
    // 声明与魔数冲突时以魔数为准）；Content-Type 一律以魔数为准。
    let key = match object_key_for(bytes, content_type, source_url) {
        Some(k) if k.ends_with(&format!(".{}", detected.ext())) => k,
        _ => format!("logos/{}.{}", sha256_hex(bytes), detected.ext()),
    };
    put_object(&client, cfg, &key, bytes, Some(detected.content_type())).await?;
    Ok(cfg.public_url(&key))
}

/// 生成并上传两档缩略图，成功返回 256px 档的公开地址；任一步失败
/// 返回 `None`（调用方回退原图转存）。两档都上传成功才算成功，
/// 避免 `logo_url` 指向的 256px 档缺失而 128px 档孤悬。
async fn try_upload_thumbnails(
    client: &reqwest::Client,
    cfg: &S3Config,
    bytes: &[u8],
    format: image::ImageFormat,
    detected: DetectedImage,
) -> Option<String> {
    let (thumb128, thumb256) = make_thumbnails(bytes, format)?;
    let key128 = thumbnail_key_for(&thumb128, detected.ext());
    let key256 = thumbnail_key_for(&thumb256, detected.ext());
    let content_type = detected.content_type();
    put_object(client, cfg, &key128, &thumb128, Some(content_type))
        .await
        .ok()?;
    put_object(client, cfg, &key256, &thumb256, Some(content_type))
        .await
        .ok()?;
    Some(cfg.public_url(&key256))
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
        // M-E：SSRF 固定 IP 的路径不走环境代理——否则 http_proxy 下
        // 请求走 CONNECT 隧道、DNS 由代理做，`resolve` 的固定值失效。
        .no_proxy()
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
    use image::GenericImageView;
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

    /// m9：2000px 台标生成两档缩略图，最长边不超过目标档；同时
    /// 覆盖 PNG/JPEG/GIF 三种可解码格式。
    #[test]
    fn thumbnails_fit_within_both_targets() {
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::Gif,
        ] {
            let bytes = test_image_bytes(format, 2000, 1200);
            assert_eq!(
                detect_image_format(&bytes).unwrap().thumbnail_format(),
                Some(format)
            );
            let (t128, t256) = make_thumbnails(&bytes, format).unwrap();
            let d128 = image::load_from_memory(&t128).unwrap().dimensions();
            let d256 = image::load_from_memory(&t256).unwrap().dimensions();
            assert!(
                d128.0 <= 128 && d128.1 <= 128,
                "{format:?} 128px档超限：{d128:?}"
            );
            assert!(
                d256.0 <= 256 && d256.1 <= 256,
                "{format:?} 256px档超限：{d256:?}"
            );
            // 2000x1200 横图：最长边顶满目标档、短边等比缩小。
            assert_eq!(d128, (128, 77), "{format:?} 128px档尺寸");
            assert_eq!(d256, (256, 154), "{format:?} 256px档尺寸");
            // 缩略图键形如 logos/thumbs/<sha256>.<ext>。
            assert!(thumbnail_key_for(&t256, "png").starts_with("logos/thumbs/"));
            assert_eq!(LOGO_THUMB_URL_SIZE, 256);
        }
    }

    /// N9：SVG 与魔数不符的字节一律拒绝转存；WebP/ICO 只认魔数、
    /// 不做缩略图。
    #[test]
    fn svg_and_bad_magic_are_rejected() {
        // 带 <script> 的 SVG：无魔数 → 拒绝；白名单也不再认 svg。
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>";
        assert_eq!(detect_image_format(svg), None);
        assert!(
            object_key_for(svg, Some("image/svg+xml"), "https://cdn.example.com/a.svg").is_none()
        );
        assert!(object_key_for(svg, None, "https://cdn.example.com/a.svg").is_none());
        // HTML 冒充图片、随机字节、截断的 PNG 签名同样拒绝。
        assert_eq!(detect_image_format(b"<html><body>hi</body></html>"), None);
        assert_eq!(detect_image_format(b"not an image at all"), None);
        assert_eq!(detect_image_format(b"\x89PNG"), None);
        assert_eq!(detect_image_format(b""), None);
        // 魔数白名单五种都认；WebP/ICO 不做缩略图（无解码器）。
        assert_eq!(
            detect_image_format(b"\x89PNG\r\n\x1a\nxxxx"),
            Some(DetectedImage::Png)
        );
        assert_eq!(
            detect_image_format(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(DetectedImage::Jpeg)
        );
        assert_eq!(detect_image_format(b"GIF89aXXXX"), Some(DetectedImage::Gif));
        let webp = b"RIFF\x00\x00\x00\x00WEBPxxxx";
        assert_eq!(detect_image_format(webp), Some(DetectedImage::Webp));
        assert_eq!(detect_image_format(webp).unwrap().thumbnail_format(), None);
        let ico = [0x00, 0x00, 0x01, 0x00, 0x01];
        assert_eq!(detect_image_format(&ico), Some(DetectedImage::Ico));
        assert_eq!(detect_image_format(&ico).unwrap().thumbnail_format(), None);
    }

    /// 5MB 上限仍在（`fetch_logo_checked` 的流式断流逻辑未动，
    /// 这里只锁定上限常量；经 stub 的超大体下载用例需开
    /// `IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS` 开关，与同进程并行测试
    /// 竞态，见 ssrf.rs 说明，故不在单测里做）。
    #[test]
    fn logo_size_cap_is_still_5mb() {
        assert_eq!(LOGO_MAX_BYTES, 5 * 1024 * 1024);
    }

    fn test_image_bytes(format: image::ImageFormat, w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), format)
            .unwrap();
        buf
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

    /// 把 `http_proxy`/`https_proxy` 指向一个死代理的 RAII 守卫
    ///（M-E 回归用；drop 时恢复原值）。
    struct HttpProxyGuard {
        prev_http: Option<String>,
        prev_https: Option<String>,
    }

    impl HttpProxyGuard {
        fn set_dead_proxy() -> Self {
            let prev_http = std::env::var("http_proxy").ok();
            let prev_https = std::env::var("https_proxy").ok();
            // 127.0.0.1:9 是 discard 端口、必然拒绝连接：若客户端还
            // 走环境代理，请求必失败；直连则不受影响。
            std::env::set_var("http_proxy", "http://127.0.0.1:9");
            std::env::set_var("https_proxy", "http://127.0.0.1:9");
            Self {
                prev_http,
                prev_https,
            }
        }
    }

    impl Drop for HttpProxyGuard {
        fn drop(&mut self) {
            match &self.prev_http {
                Some(v) => std::env::set_var("http_proxy", v),
                None => std::env::remove_var("http_proxy"),
            }
            match &self.prev_https {
                Some(v) => std::env::set_var("https_proxy", v),
                None => std::env::remove_var("https_proxy"),
            }
        }
    }

    /// M-E 回归：`http_proxy` 指向死代理时，固定后的客户端仍直连
    /// 校验过的 IP（`.no_proxy()` 生效）；若还走代理，CONNECT 到
    /// 死代理即失败。
    #[tokio::test]
    async fn pinned_client_ignores_http_proxy() {
        let _proxy = HttpProxyGuard::set_dead_proxy();
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
