//! LLBot WebUI login adapter, independent of the Milky transport credentials.
//! Contract: LuckyLilliaBot dc2b2b90092ef0f571b5546878288b83fff1e436,
//! src/webui/BE/{auth.ts,passwordHash.ts,routes/login.ts} and
//! src/main/qqProtocol/base.ts. Only status and QR retrieval are exposed.
use crate::{config::QqLoginConfig, milky::read_bounded, types::now};
use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, Url, header::HeaderValue};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

const MAX_BODY_BYTES: usize = 512 * 1024;
const MAX_QR_BYTES: usize = 256 * 1024;
const PNG_DATA_PREFIX: &str = "data:image/png;base64,";

#[derive(Clone)]
pub struct QqClient {
    http: Client,
    base: Url,
    token: HeaderValue,
}

#[derive(Debug, Serialize)]
pub struct LoginStatus {
    pub online: bool,
    pub uin: String,
    pub nick: String,
}

#[derive(Debug, Serialize)]
pub struct LoginQrCode {
    pub image: String,
    pub expires_at: Option<i64>,
    pub poll_interval_ms: u64,
}

impl QqClient {
    pub fn new(config: QqLoginConfig) -> Result<Self> {
        ensure!(config.enabled, "QQ 登录管理尚未启用");
        let mut base =
            Url::parse(&config.url).map_err(|_| anyhow::anyhow!("LLBot 管理地址无效"))?;
        ensure!(
            matches!(base.scheme(), "http" | "https")
                && base.host_str().is_some()
                && base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none(),
            "LLBot 管理地址必须为不含用户信息、查询参数和片段的 HTTP(S) 地址"
        );
        ensure!(
            !config.token.is_empty() && config.token.len() <= 4096,
            "请配置有效的 LLBot WebUI 密码"
        );
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let mut token =
            HeaderValue::from_str(&format!("{:x}", Sha256::digest(config.token.as_bytes())))
                .map_err(|_| anyhow::anyhow!("无法初始化 LLBot 管理认证"))?;
        token.set_sensitive(true);
        // A separate client guarantees custom authentication headers never follow
        // a redirect, even if the bot's shared HTTP client changes its policy.
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(1)
            .build()
            .map_err(|_| anyhow::anyhow!("无法初始化 LLBot 管理连接"))?;
        Ok(Self { http, base, token })
    }

    async fn get(&self, endpoint: &str) -> Result<Value> {
        let url = self
            .base
            .join(endpoint)
            .map_err(|_| anyhow::anyhow!("LLBot 管理地址无效"))?;
        let response = self
            .http
            .get(url)
            .header("X-Webui-Token", self.token.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("无法连接 LLBot 管理服务，请检查地址与服务状态"))?;
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            bail!("LLBot 管理认证失败，请检查 WebUI 密码；暂勿反复重试");
        }
        ensure!(!status.is_redirection(), "LLBot 管理接口不允许重定向");
        ensure!(
            status.is_success(),
            "LLBot 管理请求失败（HTTP {}）",
            status.as_u16()
        );
        let bytes = read_bounded(response, MAX_BODY_BYTES)
            .await
            .map_err(|_| anyhow::anyhow!("LLBot 管理响应过大或不完整"))?;
        let response: Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("LLBot 管理响应格式无效"))?;
        ensure!(
            response["success"].as_bool() == Some(true),
            "LLBot 未完成请求，请检查其官方管理页面中的登录与前置授权状态"
        );
        ensure!(response["data"].is_object(), "LLBot 管理响应缺少有效数据");
        Ok(response["data"].clone())
    }

    pub async fn status(&self) -> Result<LoginStatus> {
        let data = self.get("api/login-info").await?;
        let online = data["online"]
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("LLBot 登录状态格式无效"))?;
        let uin = match &data["uin"] {
            Value::String(value) => value.clone(),
            Value::Number(value) if value.as_i64().is_some_and(|id| id > 0) => value.to_string(),
            _ => bail!("LLBot 账号信息格式无效"),
        };
        ensure!(
            (!online && uin.is_empty())
                || (uin.len() <= 20
                    && uin.bytes().all(|b| b.is_ascii_digit())
                    && uin.parse::<i64>().is_ok_and(|id| id > 0)),
            "LLBot 账号信息格式无效"
        );
        let nick = data["nick"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("LLBot 昵称格式无效"))?;
        ensure!(nick.chars().count() <= 256, "LLBot 昵称超出长度限制");
        Ok(LoginStatus {
            online,
            uin,
            nick: nick.to_owned(),
        })
    }

    pub async fn qrcode(&self) -> Result<LoginQrCode> {
        let data = self.get("api/login-qrcode").await?;
        let image = data["pngBase64QrcodeData"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("LLBot 尚未提供登录二维码，请检查其官方管理页面"))?;
        let encoded = image.strip_prefix(PNG_DATA_PREFIX).unwrap_or(image);
        ensure!(
            encoded.len() <= MAX_QR_BYTES.div_ceil(3) * 4,
            "LLBot 二维码图片超出大小限制"
        );
        let png = STANDARD
            .decode(encoded)
            .map_err(|_| anyhow::anyhow!("LLBot 二维码不是有效 PNG 图片"))?;
        validate_png(&png)?;
        let expires_at = match data.get("expireTime") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let seconds = value
                    .as_i64()
                    .filter(|seconds| (1..=3600).contains(seconds))
                    .ok_or_else(|| anyhow::anyhow!("LLBot 二维码有效期无效或已过期"))?;
                Some(now() + seconds)
            }
        };
        let poll_seconds = match data.get("pollTimeInterval") {
            None | Some(Value::Null) => 3,
            Some(value) => value
                .as_u64()
                .filter(|seconds| (1..=30).contains(seconds))
                .ok_or_else(|| anyhow::anyhow!("LLBot 登录轮询间隔无效"))?,
        };
        Ok(LoginQrCode {
            image: format!("{PNG_DATA_PREFIX}{}", STANDARD.encode(png)),
            expires_at,
            poll_interval_ms: poll_seconds * 1000,
        })
    }
}

fn validate_png(png: &[u8]) -> Result<()> {
    ensure!(
        png.len() <= MAX_QR_BYTES && png.starts_with(b"\x89PNG\r\n\x1a\n"),
        "LLBot 二维码不是有效 PNG 图片"
    );
    let mut offset = 8;
    let mut saw_image = false;
    while png.len().saturating_sub(offset) >= 12 {
        let length = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|n| n.checked_add(length))
            .filter(|end| *end <= png.len())
            .ok_or_else(|| anyhow::anyhow!("LLBot 二维码 PNG 数据不完整"))?;
        let kind = &png[offset + 4..offset + 8];
        if offset == 8 {
            ensure!(kind == b"IHDR" && length == 13, "LLBot 二维码 PNG 头部无效");
            let width = u32::from_be_bytes(png[offset + 8..offset + 12].try_into().unwrap());
            let height = u32::from_be_bytes(png[offset + 12..offset + 16].try_into().unwrap());
            ensure!(
                (1..=2048).contains(&width) && (1..=2048).contains(&height),
                "LLBot 二维码尺寸超出限制"
            );
        } else {
            ensure!(kind != b"IHDR", "LLBot 二维码 PNG 头部重复");
        }
        if kind == b"IDAT" {
            saw_image = true;
        }
        if kind == b"IEND" {
            ensure!(
                saw_image && length == 0 && end == png.len(),
                "LLBot 二维码 PNG 结尾无效"
            );
            return Ok(());
        }
        offset = end;
    }
    bail!("LLBot 二维码 PNG 数据不完整")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=";

    async fn server(
        status: &str,
        headers: &str,
        body: String,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        );
        let task = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(read > 0 && bytes.len() < 16384);
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = stream.write_all(response.as_bytes()).await;
            String::from_utf8(bytes).unwrap()
        });
        (url, task)
    }

    fn client(url: String) -> QqClient {
        QqClient::new(QqLoginConfig {
            enabled: true,
            url,
            token: "test-password".into(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn status_uses_separate_hashed_auth_and_only_returns_account_fields() {
        let body = json!({"success":true,"data":{"online":true,"uin":"123","nick":"机器人","token":"never-return","webui":{"host":"secret"}}}).to_string();
        let (url, server) = server("200 OK", "", body).await;
        let status = client(url).status().await.unwrap();
        assert!(status.online);
        assert_eq!(status.uin, "123");
        assert_eq!(status.nick, "机器人");
        assert_eq!(
            serde_json::to_value(status)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            3
        );
        let request = server.await.unwrap();
        assert!(request.starts_with("GET /api/login-info "));
        assert!(request.to_lowercase().contains(&format!(
            "x-webui-token: {:x}",
            Sha256::digest(b"test-password")
        )));
        assert!(!request.contains("test-password"));
        assert!(!request.to_lowercase().contains("authorization:"));
    }

    #[tokio::test]
    async fn qrcode_accepts_png_and_converts_relative_expiry_without_exposing_upstream_url() {
        let body = json!({"success":true,"data":{"pngBase64QrcodeData":format!("{PNG_DATA_PREFIX}{PNG}"),"qrcodeUrl":"https://sensitive.example/?token=secret","expireTime":180,"pollTimeInterval":3}}).to_string();
        let (url, server) = server("200 OK", "", body).await;
        let before = now();
        let result = client(url).qrcode().await.unwrap();
        assert_eq!(result.image, format!("{PNG_DATA_PREFIX}{PNG}"));
        assert!((before + 180..=now() + 180).contains(&result.expires_at.unwrap()));
        assert_eq!(result.poll_interval_ms, 3000);
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("sensitive")
        );
        assert!(server.await.unwrap().starts_with("GET /api/login-qrcode "));
    }

    #[tokio::test]
    async fn failed_envelopes_bad_json_and_auth_errors_are_rejected_without_secret_details() {
        for (status, body) in [
            (
                "200 OK",
                r#"{"success":false,"message":"password-secret https://private/?token=secret"}"#,
            ),
            ("200 OK", r#"{"success":"true","data":{}}"#),
            (
                "200 OK",
                r#"{"success":true,"data":{"online":true,"uin":"","nick":""}}"#,
            ),
            ("200 OK", "not json password-secret"),
            ("403 Forbidden", "password-secret"),
        ] {
            let (url, server) = server(status, "", body.into()).await;
            let error = client(url).status().await.unwrap_err().to_string();
            assert!(!error.contains("password-secret") && !error.contains("https://private"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn oversized_responses_and_non_png_qrcodes_are_rejected() {
        let (url, task) = server("200 OK", "", "x".repeat(MAX_BODY_BYTES + 1)).await;
        assert!(
            client(url)
                .status()
                .await
                .unwrap_err()
                .to_string()
                .contains("过大")
        );
        task.await.unwrap();
        let mut huge_dimension = STANDARD.decode(PNG).unwrap();
        huge_dimension[16..20].copy_from_slice(&4096u32.to_be_bytes());
        for invalid in [
            "https://remote.example/qrcode?token=secret".to_owned(),
            "data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=".to_owned(),
            format!("{PNG_DATA_PREFIX}{}", STANDARD.encode(b"not png")),
            format!(
                "{PNG_DATA_PREFIX}{}",
                STANDARD.encode(&STANDARD.decode(PNG).unwrap()[..30])
            ),
            format!("{PNG_DATA_PREFIX}{}", STANDARD.encode(huge_dimension)),
        ] {
            let (url, task) = server(
                "200 OK",
                "",
                json!({"success":true,"data":{"pngBase64QrcodeData":invalid}}).to_string(),
            )
            .await;
            assert!(client(url).qrcode().await.is_err());
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn redirects_never_receive_management_credentials() {
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let location = format!(
            "Location: http://{}/stolen\r\n",
            destination.local_addr().unwrap()
        );
        let (url, task) = server("302 Found", &location, String::new()).await;
        assert!(
            client(url)
                .status()
                .await
                .unwrap_err()
                .to_string()
                .contains("重定向")
        );
        task.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), destination.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn unsafe_management_urls_are_rejected_without_echoing_them() {
        for url in [
            "ftp://localhost/",
            "http://user:password-secret@localhost/",
            "http://localhost/?token=password-secret",
            "http://localhost/#password-secret",
        ] {
            let result = QqClient::new(QqLoginConfig {
                enabled: true,
                url: url.into(),
                token: "ok".into(),
            });
            assert!(
                !result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("password-secret")
            );
        }
    }
}
