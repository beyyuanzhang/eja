use crate::config::write_json_atomic;
use crate::http::{get_bytes, post_bytes, truncate_utf8};
use crate::sandbox::TrustedDir;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
    Arc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const GITHUB_PUBLIC_CLIENT_ID: &str = "178c6fc778ccc68e1d6a";
const OAUTH_HTTP_TIMEOUT_SECS: u64 = 15;
const LOOPBACK_WAIT_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthToken {
    pub url: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl OAuthToken {
    pub fn matches_url(&self, url: &str) -> bool {
        !self.access_token.is_empty() && normalize_url(&self.url) == normalize_url(url)
    }

    pub fn is_expired(&self, now_unix: i64) -> bool {
        self.expires_at.is_some_and(|exp| now_unix + 60 >= exp)
    }
}

pub struct OAuthStore {
    core_dir: Option<TrustedDir>,
    tokens: BTreeMap<String, OAuthToken>,
}

impl OAuthStore {
    pub fn load(core_dir: Option<TrustedDir>) -> Self {
        let tokens = core_dir
            .as_ref()
            .and_then(|dir| dir.read_file("oauth.json").ok())
            .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, OAuthToken>>(&bytes).ok())
            .unwrap_or_default();
        Self { core_dir, tokens }
    }

    pub fn is_authorized(&self, id: &str, url: &str) -> bool {
        self.tokens
            .get(id)
            .is_some_and(|token| token.matches_url(url))
    }

    pub fn access_token(&mut self, id: &str, url: &str) -> Option<String> {
        let token = self.tokens.get(id)?.clone();
        if !token.matches_url(url) {
            return None;
        }
        if token.is_expired(now_unix()) {
            if let Some(refreshed) = self.refresh(id, url) {
                return Some(refreshed);
            }
        }
        Some(token.access_token)
    }

    pub fn refresh(&mut self, id: &str, url: &str) -> Option<String> {
        let current = self.tokens.get(id)?.clone();
        if !current.matches_url(url) {
            return None;
        }
        let updated = refresh_oauth_token(&current).ok()?;
        let access = updated.access_token.clone();
        let _ = self.save(id, updated);
        Some(access)
    }

    pub fn save(&mut self, id: &str, token: OAuthToken) -> Result<(), String> {
        self.tokens.insert(id.to_string(), token);
        if let Some(dir) = &self.core_dir {
            write_json_atomic(dir, "oauth.json", &self.tokens)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum OAuthEvent {
    Started {
        id: String,
        #[allow(dead_code)]
        auth_url: String,
        oauth_code: Option<String>,
    },
    Finished {
        id: String,
        result: Result<OAuthToken, String>,
    },
}

#[derive(Debug, Clone)]
pub struct OAuthMetadata {
    pub issuer: String,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub device_authorization_endpoint: Option<String>,
    pub scopes: Vec<String>,
}

pub fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn is_auth_error(message: &str) -> bool {
    message.contains("MCP HTTP 401")
        || message.contains("MCP HTTP 403")
        || message.contains("Unauthorized")
        || message.contains("invalid_token")
}

pub fn discover_oauth_metadata(
    mcp_url: &str,
    custom_headers: &BTreeMap<String, String>,
) -> Result<OAuthMetadata, String> {
    let (origin, path) = split_origin_and_path(mcp_url)
        .ok_or_else(|| format!("无效的 MCP URL: {}", mcp_url.trim()))?;
    let json_headers = vec![("Accept".to_string(), "application/json".to_string())];

    let mut resource_urls = Vec::new();
    let trimmed_path = path.trim_end_matches('/');
    if !trimmed_path.is_empty() {
        push_unique(
            &mut resource_urls,
            format!("{origin}/.well-known/oauth-protected-resource{path}"),
        );
        push_unique(
            &mut resource_urls,
            format!("{origin}/.well-known/oauth-protected-resource{trimmed_path}"),
        );
    }
    push_unique(
        &mut resource_urls,
        format!("{origin}/.well-known/oauth-protected-resource"),
    );

    let mut resource_meta: Option<Value> = None;
    for candidate in &resource_urls {
        if let Some(val) = fetch_json_object(candidate, &json_headers) {
            if val.get("authorization_servers").is_some() || val.get("resource").is_some() {
                resource_meta = Some(val);
                break;
            }
        }
    }

    if resource_meta.is_none() {
        let mut probe_headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        for (k, v) in custom_headers {
            probe_headers.push((k.clone(), v.clone()));
        }
        if let Ok(resp) = post_bytes(mcp_url, &probe_headers, b"{}", OAUTH_HTTP_TIMEOUT_SECS) {
            if let Some(meta_url) =
                extract_www_authenticate_param(&resp.headers, "resource_metadata")
            {
                resource_meta = fetch_json_object(&meta_url, &json_headers);
            }
        }
    }

    let mut issuer = origin.clone();
    let mut scopes = Vec::new();
    if let Some(res) = &resource_meta {
        if let Some(first_server) = res
            .get("authorization_servers")
            .and_then(Value::as_array)
            .and_then(|arr| arr.iter().find_map(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            issuer = first_server.trim_end_matches('/').to_string();
        }
        scopes = extract_string_array(res.get("scopes_supported"));
    }

    let (iss_origin, iss_path) =
        split_origin_and_path(&issuer).unwrap_or_else(|| (origin.clone(), String::new()));
    let iss_path_trimmed = iss_path.trim_end_matches('/');
    let mut auth_server_urls = Vec::new();
    if !iss_path_trimmed.is_empty() {
        push_unique(
            &mut auth_server_urls,
            format!("{iss_origin}/.well-known/oauth-authorization-server{iss_path_trimmed}"),
        );
        push_unique(
            &mut auth_server_urls,
            format!("{iss_origin}/.well-known/openid-configuration{iss_path_trimmed}"),
        );
        push_unique(
            &mut auth_server_urls,
            format!("{iss_origin}{iss_path_trimmed}/.well-known/openid-configuration"),
        );
    }
    push_unique(
        &mut auth_server_urls,
        format!("{iss_origin}/.well-known/oauth-authorization-server"),
    );
    push_unique(
        &mut auth_server_urls,
        format!("{iss_origin}/.well-known/openid-configuration"),
    );
    if iss_origin != origin {
        push_unique(
            &mut auth_server_urls,
            format!("{origin}/.well-known/oauth-authorization-server"),
        );
    }

    for candidate in &auth_server_urls {
        if let Some(val) = fetch_json_object(candidate, &json_headers) {
            let token_ep = val
                .get("token_endpoint")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let auth_ep = val
                .get("authorization_endpoint")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            let reg_ep = val
                .get("registration_endpoint")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            let device_ep = val
                .get("device_authorization_endpoint")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            if let Some(token_endpoint) = token_ep {
                if scopes.is_empty() {
                    scopes = extract_string_array(val.get("scopes_supported"));
                }
                return Ok(OAuthMetadata {
                    issuer,
                    authorization_endpoint: auth_ep,
                    token_endpoint: token_endpoint.to_string(),
                    registration_endpoint: reg_ep,
                    device_authorization_endpoint: device_ep,
                    scopes,
                });
            }
        }
    }

    Ok(OAuthMetadata {
        issuer: iss_origin.clone(),
        authorization_endpoint: Some(format!("{iss_origin}/authorize")),
        token_endpoint: format!("{iss_origin}/token"),
        registration_endpoint: Some(format!("{iss_origin}/register")),
        device_authorization_endpoint: None,
        scopes,
    })
}

pub fn spawn_oauth_flow(
    id: String,
    mcp_url: String,
    headers: BTreeMap<String, String>,
    tx: Sender<OAuthEvent>,
) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&cancel);
    thread::spawn(move || {
        let result = run_oauth_flow(&id, &mcp_url, &headers, &cancel_flag, &tx);
        if !cancel_flag.load(Ordering::Relaxed) {
            let _ = tx.send(OAuthEvent::Finished { id, result });
        }
    });
    cancel
}

fn run_oauth_flow(
    id: &str,
    mcp_url: &str,
    headers: &BTreeMap<String, String>,
    cancel: &AtomicBool,
    tx: &Sender<OAuthEvent>,
) -> Result<OAuthToken, String> {
    let meta = discover_oauth_metadata(mcp_url, headers)?;
    if cancel.load(Ordering::Relaxed) {
        return Err("授权已取消".into());
    }

    let custom_client_id = find_header_ci(headers, &["x-oauth-client-id", "client_id"]);
    let custom_client_secret = find_header_ci(headers, &["x-oauth-client-secret", "client_secret"]);

    if let Some(auth_endpoint) = &meta.authorization_endpoint {
        if custom_client_id.is_some() || meta.registration_endpoint.is_some() {
            let listener = TcpListener::bind("127.0.0.1:0")
                .map_err(|err| format!("启动本地 OAuth 回调监听失败: {err}"))?;
            let port = listener
                .local_addr()
                .map_err(|err| format!("读取本地回调端口失败: {err}"))?
                .port();
            let redirect_uri = format!("http://127.0.0.1:{port}/callback");

            let registered = if let Some(cid) = custom_client_id.clone() {
                Some((cid, custom_client_secret.clone()))
            } else if let Some(reg_endpoint) = &meta.registration_endpoint {
                register_dynamic_client(reg_endpoint, &redirect_uri).ok()
            } else {
                None
            };

            if let Some((client_id, client_secret)) = registered {
                return run_pkce_loopback_flow(
                    id,
                    mcp_url,
                    &meta,
                    auth_endpoint,
                    listener,
                    &redirect_uri,
                    client_id,
                    client_secret,
                    cancel,
                    tx,
                );
            }
        }
    }

    if let Some(device_endpoint) = &meta.device_authorization_endpoint {
        let client_id = custom_client_id
            .or_else(|| {
                if meta.issuer.contains("github.com") || device_endpoint.contains("github.com") {
                    Some(GITHUB_PUBLIC_CLIENT_ID.to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                "该连接器要求预注册 OAuth Client ID（可在连接器键值中填入 X-OAuth-Client-Id=...）"
                    .to_string()
            })?;
        return run_device_code_flow(
            id,
            mcp_url,
            &meta,
            device_endpoint,
            &client_id,
            custom_client_secret.as_deref(),
            cancel,
            tx,
        );
    }

    Err("该连接器未提供可用的 OAuth 动态注册或设备码授权端点".into())
}

#[allow(clippy::too_many_arguments)]
fn run_pkce_loopback_flow(
    id: &str,
    mcp_url: &str,
    meta: &OAuthMetadata,
    auth_endpoint: &str,
    listener: TcpListener,
    redirect_uri: &str,
    client_id: String,
    client_secret: Option<String>,
    cancel: &AtomicBool,
    tx: &Sender<OAuthEvent>,
) -> Result<OAuthToken, String> {
    let verifier = random_code_verifier();
    let challenge = pkce_s256_challenge(&verifier);
    let state = random_hex(16);
    let sep = if auth_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    let auth_url = format!(
        "{auth_endpoint}{sep}response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        url_encode(&client_id),
        url_encode(redirect_uri),
        url_encode(&challenge),
        url_encode(&state)
    );

    open_browser(&auth_url);
    let _ = tx.send(OAuthEvent::Started {
        id: id.to_string(),
        auth_url,
        oauth_code: None,
    });

    let code = wait_for_loopback_code(listener, &state, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return Err("授权已取消".into());
    }

    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", client_id.clone()),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = &client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let body = encode_form(&form);
    let resp = post_bytes(
        &meta.token_endpoint,
        &[
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("Accept".to_string(), "application/json".to_string()),
        ],
        body.as_bytes(),
        OAUTH_HTTP_TIMEOUT_SECS,
    )
    .map_err(|err| format!("OAuth 换取 Token 请求失败: {err:?}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "OAuth 换取 Token 失败 (HTTP {}): {}",
            resp.status,
            truncate_utf8(resp.body.trim(), 200)
        ));
    }
    parse_token_response(
        &resp.body,
        mcp_url,
        &meta.token_endpoint,
        Some(client_id),
        client_secret,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_device_code_flow(
    id: &str,
    mcp_url: &str,
    meta: &OAuthMetadata,
    device_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    cancel: &AtomicBool,
    tx: &Sender<OAuthEvent>,
) -> Result<OAuthToken, String> {
    let mut form = vec![("client_id", client_id.to_string())];
    if !meta.scopes.is_empty() {
        form.push(("scope", meta.scopes.join(" ")));
    }
    let body = encode_form(&form);
    let resp = post_bytes(
        device_endpoint,
        &[
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("Accept".to_string(), "application/json".to_string()),
        ],
        body.as_bytes(),
        OAUTH_HTTP_TIMEOUT_SECS,
    )
    .map_err(|err| format!("请求 OAuth 设备码失败: {err:?}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "请求 OAuth 设备码失败 (HTTP {}): {}",
            resp.status,
            truncate_utf8(resp.body.trim(), 200)
        ));
    }
    let payload = parse_json_or_form(&resp.body)
        .ok_or_else(|| "无法解析 OAuth 设备码响应".to_string())?;
    let device_code = payload
        .get("device_code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("设备码响应缺少 device_code")?
        .to_string();
    let user_code = payload
        .get("user_code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("设备码响应缺少 user_code")?
        .to_string();
    let verification_uri = payload
        .get("verification_uri_complete")
        .or_else(|| payload.get("verification_uri"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("设备码响应缺少 verification_uri")?
        .to_string();
    let mut interval_secs = payload
        .get("interval")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .max(1);
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(600)
        .max(30);

    copy_to_clipboard(&user_code);
    open_browser(&verification_uri);
    let _ = tx.send(OAuthEvent::Started {
        id: id.to_string(),
        auth_url: verification_uri,
        oauth_code: Some(user_code),
    });

    let deadline = Instant::now() + Duration::from_secs(expires_in);
    while Instant::now() < deadline {
        let wait_until = Instant::now() + Duration::from_secs(interval_secs);
        while Instant::now() < wait_until {
            if cancel.load(Ordering::Relaxed) {
                return Err("授权已取消".into());
            }
            thread::sleep(Duration::from_millis(100));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("授权已取消".into());
        }

        let mut poll_form = vec![
            ("client_id", client_id.to_string()),
            ("device_code", device_code.clone()),
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            ),
        ];
        if let Some(secret) = client_secret {
            poll_form.push(("client_secret", secret.to_string()));
        }
        let poll_body = encode_form(&poll_form);
        let Ok(poll_resp) = post_bytes(
            &meta.token_endpoint,
            &[
                (
                    "Content-Type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("Accept".to_string(), "application/json".to_string()),
            ],
            poll_body.as_bytes(),
            OAUTH_HTTP_TIMEOUT_SECS,
        ) else {
            continue;
        };
        let Some(val) = parse_json_or_form(&poll_resp.body) else {
            continue;
        };
        if val
            .get("access_token")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            return parse_token_response(
                &poll_resp.body,
                mcp_url,
                &meta.token_endpoint,
                Some(client_id.to_string()),
                client_secret.map(ToString::to_string),
            );
        }
        match val.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval_secs += 5;
                continue;
            }
            Some("expired_token") => return Err("OAuth 设备授权码已过期，请重新点击授权".into()),
            Some("access_denied") => return Err("OAuth 授权被拒绝".into()),
            Some(other) => {
                let desc = val
                    .get("error_description")
                    .and_then(Value::as_str)
                    .unwrap_or(other);
                return Err(format!("OAuth 授权失败: {desc}"));
            }
            None => continue,
        }
    }
    Err("OAuth 授权等待超时".into())
}

fn register_dynamic_client(
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<(String, Option<String>), String> {
    let payload = json!({
        "client_name": "EJA",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none"
    });
    let body = serde_json::to_vec(&payload).map_err(|err| err.to_string())?;
    let resp = post_bytes(
        registration_endpoint,
        &[
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ],
        &body,
        OAUTH_HTTP_TIMEOUT_SECS,
    )
    .map_err(|err| format!("OAuth 动态注册请求失败: {err:?}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("OAuth 动态注册失败 (HTTP {})", resp.status));
    }
    let val = serde_json::from_str::<Value>(&resp.body)
        .map_err(|err| format!("解析动态注册响应失败: {err}"))?;
    let client_id = val
        .get("client_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("动态注册响应缺少 client_id")?
        .to_string();
    let client_secret = val
        .get("client_secret")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    Ok((client_id, client_secret))
}

fn wait_for_loopback_code(
    listener: TcpListener,
    expected_state: &str,
    cancel: &AtomicBool,
) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("设置本地回调监听失败: {err}"))?;
    let deadline = Instant::now() + LOOPBACK_WAIT_TIMEOUT;
    while Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            return Err("授权已取消".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                if path.starts_with("/favicon.ico") {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
                let params = parse_query_params(query);
                if let Some(err) = params.get("error") {
                    let desc = params.get("error_description").unwrap_or(err);
                    write_loopback_html(&mut stream, false, "授权未完成，请返回 EJA。");
                    return Err(format!("OAuth 授权失败: {desc}"));
                }
                let Some(code) = params.get("code").filter(|s| !s.is_empty()).cloned() else {
                    write_loopback_html(&mut stream, false, "缺少授权码，请返回 EJA 重试。");
                    continue;
                };
                if params.get("state").map(String::as_str) != Some(expected_state) {
                    write_loopback_html(&mut stream, false, "状态校验不一致，请返回 EJA 重试。");
                    return Err("OAuth state 校验不一致".into());
                }
                write_loopback_html(&mut stream, true, "授权完成，可关闭此页面返回 EJA。");
                return Ok(code);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(format!("接收本地 OAuth 回调失败: {err}")),
        }
    }
    Err("OAuth 浏览器授权等待超时".into())
}

fn write_loopback_html(stream: &mut impl Write, ok: bool, message: &str) {
    let status = if ok { "200 OK" } else { "400 Bad Request" };
    let script = if ok {
        "<script>setTimeout(()=>window.close(),300)</script>"
    } else {
        ""
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>EJA</title></head>\
        <body style=\"font-family:-apple-system,system-ui,sans-serif;display:flex;align-items:center;justify-content:center;height:80vh;margin:0;color:#222;font-size:14px\">\
        {script}<p>{message}</p></body></html>"
    );
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn refresh_oauth_token(current: &OAuthToken) -> Result<OAuthToken, String> {
    let refresh_token = current
        .refresh_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("缺少 refresh_token")?;
    let token_endpoint = current
        .token_endpoint
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("缺少 token_endpoint")?;
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    if let Some(cid) = &current.client_id {
        form.push(("client_id", cid.clone()));
    }
    if let Some(secret) = &current.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let body = encode_form(&form);
    let resp = post_bytes(
        token_endpoint,
        &[
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("Accept".to_string(), "application/json".to_string()),
        ],
        body.as_bytes(),
        OAUTH_HTTP_TIMEOUT_SECS,
    )
    .map_err(|err| format!("刷新 OAuth Token 请求失败: {err:?}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("刷新 OAuth Token 失败 (HTTP {})", resp.status));
    }
    let mut next = parse_token_response(
        &resp.body,
        &current.url,
        token_endpoint,
        current.client_id.clone(),
        current.client_secret.clone(),
    )?;
    if next.refresh_token.is_none() {
        next.refresh_token = current.refresh_token.clone();
    }
    Ok(next)
}

fn parse_token_response(
    raw_body: &str,
    mcp_url: &str,
    token_endpoint: &str,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Result<OAuthToken, String> {
    let val = parse_json_or_form(raw_body)
        .ok_or_else(|| format!("无法解析 OAuth Token 响应: {}", truncate_utf8(raw_body, 200)))?;
    let access_token = val
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            let err = val
                .get("error_description")
                .or_else(|| val.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("响应缺少 access_token");
            format!("OAuth 获取 Token 失败: {err}")
        })?
        .to_string();
    let refresh_token = val
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let expires_in = val
        .get("expires_in")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse::<i64>().ok()))
        .filter(|&secs| secs > 0);
    let expires_at = expires_in.map(|secs| now_unix() + secs);

    Ok(OAuthToken {
        url: normalize_url(mcp_url),
        access_token,
        refresh_token,
        token_endpoint: Some(token_endpoint.to_string()),
        client_id,
        client_secret,
        expires_at,
    })
}

fn parse_json_or_form(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
        if val.is_object() {
            return Some(val);
        }
    }
    let params = parse_query_params(trimmed);
    if params.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::new();
    for (k, v) in params {
        map.insert(k, Value::String(v));
    }
    Some(Value::Object(map))
}

fn fetch_json_object(url: &str, headers: &[(String, String)]) -> Option<Value> {
    let resp = get_bytes(url, headers, OAUTH_HTTP_TIMEOUT_SECS).ok()?;
    if !(200..300).contains(&resp.status) {
        return None;
    }
    serde_json::from_str::<Value>(resp.body.trim())
        .ok()
        .filter(Value::is_object)
}

pub fn split_origin_and_path(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim();
    let scheme_end = trimmed.find("://")?;
    let host_start = scheme_end + 3;
    let after_scheme = &trimmed[host_start..];
    if after_scheme.is_empty() {
        return None;
    }
    match after_scheme.find('/') {
        Some(slash_idx) => {
            let origin = trimmed[..host_start + slash_idx].to_string();
            let path = after_scheme[slash_idx..].to_string();
            if origin.len() <= host_start {
                None
            } else {
                Some((origin, path))
            }
        }
        None => Some((trimmed.to_string(), String::new())),
    }
}

pub fn extract_www_authenticate_param(headers: &str, param: &str) -> Option<String> {
    for line in headers.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("www-authenticate") {
            continue;
        }
        let pattern = format!("{param}=\"");
        if let Some(start) = value.find(&pattern) {
            let rest = &value[start + pattern.len()..];
            if let Some(end) = rest.find('"') {
                let extracted = rest[..end].trim();
                if !extracted.is_empty() {
                    return Some(extracted.to_string());
                }
            }
        }
    }
    None
}

fn extract_string_array(val: Option<&Value>) -> Vec<String> {
    val.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn find_header_ci(headers: &BTreeMap<String, String>, names: &[&str]) -> Option<String> {
    for (k, v) in headers {
        if names.iter().any(|target| k.eq_ignore_ascii_case(target)) && !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

fn encode_form(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_query_params(query: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let key = url_decode(k.trim());
        if !key.is_empty() {
            map.insert(key, url_decode(v.trim()));
        }
    }
    map
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    let seed = now_unix() as u64 ^ (std::process::id() as u64);
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = ((seed.wrapping_mul(6364136223846793005).wrapping_add(i as u64)) >> 24) as u8;
    }
    buf
}

fn random_code_verifier() -> String {
    base64url_encode(&random_bytes(32))
}

fn random_hex(bytes_len: usize) -> String {
    random_bytes(bytes_len)
        .into_iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn pkce_s256_challenge(verifier: &str) -> String {
    base64url_encode(&sha256(verifier.as_bytes()))
}

pub fn base64url_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 { u32::from(chunk[1]) } else { 0 };
        let b2 = if chunk.len() > 2 { u32::from(chunk[2]) } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        }
    }
    out
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64) * 8;
    let mut padded = Vec::with_capacity((data.len() + 9).div_ceil(64) * 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (state, delta) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *state = state.wrapping_add(delta);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(not(test))]
fn open_browser(url: &str) {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
fn open_browser(_url: &str) {}

#[cfg(not(test))]
fn copy_to_clipboard(text: &str) {
    if let Ok(mut child) = std::process::Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

#[cfg(test)]
fn copy_to_clipboard(_text: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_pkce_s256_vector_and_www_authenticate_parsing() {
        assert_eq!(
            pkce_s256_challenge("abc"),
            "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0"
        );
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_s256_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );

        let headers = "HTTP/2 401\r\nwww-authenticate: Bearer error=\"invalid_request\", resource_metadata=\"https://api.githubcopilot.com/.well-known/oauth-protected-resource/mcp/\"\r\n";
        assert_eq!(
            extract_www_authenticate_param(headers, "resource_metadata").as_deref(),
            Some("https://api.githubcopilot.com/.well-known/oauth-protected-resource/mcp/")
        );
        assert_eq!(
            split_origin_and_path("https://api.githubcopilot.com/mcp/"),
            Some(("https://api.githubcopilot.com".into(), "/mcp/".into()))
        );
    }

    #[test]
    fn oauth_store_persists_in_core_dir_and_scopes_token_to_url() {
        let dir = tempfile::tempdir().unwrap();
        let core = TrustedDir::open(dir.path().to_path_buf()).unwrap();
        let mut store = OAuthStore::load(Some(core.clone()));
        store
            .save(
                "notion",
                OAuthToken {
                    url: "https://mcp.notion.com/mcp/".into(),
                    access_token: "sec_123".into(),
                    ..Default::default()
                },
            )
            .unwrap();

        let mut reloaded = OAuthStore::load(Some(core));
        assert!(reloaded.is_authorized("notion", "https://mcp.notion.com/mcp"));
        assert_eq!(
            reloaded
                .access_token("notion", "https://mcp.notion.com/mcp")
                .as_deref(),
            Some("sec_123")
        );
        assert!(!reloaded.is_authorized("notion", "https://evil.example.com/mcp"));
        assert!(reloaded
            .access_token("notion", "https://evil.example.com/mcp")
            .is_none());
    }

    #[test]
    fn device_code_flow_discovers_metadata_emits_code_and_exchanges_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let base_srv = base.clone();

        let server = thread::spawn(move || {
            for _ in 0..4 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let (status, body) = if first_line.contains("/.well-known/oauth-protected-resource")
                {
                    (
                        "200 OK",
                        json!({
                            "authorization_servers": [format!("{base_srv}/login/oauth")],
                            "scopes_supported": ["repo", "read:user"]
                        })
                        .to_string(),
                    )
                } else if first_line
                    .contains("/.well-known/oauth-authorization-server/login/oauth")
                {
                    (
                        "200 OK",
                        json!({
                            "issuer": format!("{base_srv}/login/oauth"),
                            "device_authorization_endpoint": format!("{base_srv}/login/device/code"),
                            "token_endpoint": format!("{base_srv}/login/oauth/access_token")
                        })
                        .to_string(),
                    )
                } else if first_line.contains("/login/device/code") {
                    (
                        "200 OK",
                        json!({
                            "device_code": "dev_999",
                            "user_code": "EFGH-5678",
                            "verification_uri": format!("{base_srv}/login/device"),
                            "interval": 1,
                            "expires_in": 60
                        })
                        .to_string(),
                    )
                } else {
                    (
                        "200 OK",
                        json!({
                            "access_token": "gho_test_token_123",
                            "token_type": "bearer"
                        })
                        .to_string(),
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let mut headers = BTreeMap::new();
        headers.insert("X-OAuth-Client-Id".into(), "test_cli_id".into());
        let (tx, rx) = std::sync::mpsc::channel();
        let _cancel = spawn_oauth_flow("gh".into(), format!("{base}/mcp"), headers, tx);

        let started = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            started,
            OAuthEvent::Started {
                ref id,
                oauth_code: Some(ref code),
                ..
            } if id == "gh" && code == "EFGH-5678"
        ));

        let finished = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        match finished {
            OAuthEvent::Finished { id, result } => {
                assert_eq!(id, "gh");
                let token = result.expect("expected device token");
                assert_eq!(token.access_token, "gho_test_token_123");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let _ = server.join();
    }

    #[test]
    fn pkce_loopback_flow_registers_client_and_verifies_s256_verifier() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let base_srv = base.clone();

        let server = thread::spawn(move || {
            for _ in 0..4 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let req_body = req.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                let (status, body) = if first_line.contains("/.well-known/oauth-protected-resource")
                {
                    (
                        "200 OK",
                        json!({
                            "resource": format!("{base_srv}/mcp"),
                            "authorization_servers": [base_srv]
                        })
                        .to_string(),
                    )
                } else if first_line.contains("/.well-known/oauth-authorization-server") {
                    (
                        "200 OK",
                        json!({
                            "issuer": base_srv,
                            "authorization_endpoint": format!("{base_srv}/authorize"),
                            "token_endpoint": format!("{base_srv}/token"),
                            "registration_endpoint": format!("{base_srv}/register")
                        })
                        .to_string(),
                    )
                } else if first_line.contains("/register") {
                    (
                        "201 Created",
                        json!({
                            "client_id": "dcr_client_42"
                        })
                        .to_string(),
                    )
                } else {
                    let form = parse_query_params(req_body.trim());
                    assert_eq!(
                        form.get("grant_type").map(String::as_str),
                        Some("authorization_code")
                    );
                    assert_eq!(form.get("code").map(String::as_str), Some("auth_code_777"));
                    assert_eq!(
                        form.get("client_id").map(String::as_str),
                        Some("dcr_client_42")
                    );
                    let verifier = form.get("code_verifier").expect("missing code_verifier");
                    (
                        "200 OK",
                        json!({
                            "access_token": format!("pkce_tok_{}", pkce_s256_challenge(verifier)),
                            "refresh_token": "pkce_ref_1",
                            "expires_in": 3600
                        })
                        .to_string(),
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let (tx, rx) = std::sync::mpsc::channel();
        let _cancel = spawn_oauth_flow(
            "notion".into(),
            format!("{base}/mcp"),
            BTreeMap::new(),
            tx,
        );

        let OAuthEvent::Started {
            id,
            auth_url,
            oauth_code,
        } = rx.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("expected Started event");
        };
        assert_eq!(id, "notion");
        assert!(oauth_code.is_none());

        let query = auth_url.split_once('?').unwrap().1;
        let params = parse_query_params(query);
        let redirect_uri = params.get("redirect_uri").unwrap();
        let state = params.get("state").unwrap();
        let challenge = params.get("code_challenge").unwrap();

        let callback_url = format!(
            "{redirect_uri}?code=auth_code_777&state={}",
            url_encode(state)
        );
        let cb_resp = get_bytes(&callback_url, &[], 5).unwrap();
        assert_eq!(cb_resp.status, 200);

        let OAuthEvent::Finished { id, result } =
            rx.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("expected Finished event");
        };
        assert_eq!(id, "notion");
        let token = result.expect("expected PKCE token");
        assert_eq!(token.access_token, format!("pkce_tok_{challenge}"));
        assert_eq!(token.refresh_token.as_deref(), Some("pkce_ref_1"));
        let _ = server.join();
    }
}
