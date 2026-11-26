use crate::config::write_json_atomic;
use crate::http::{get_bytes, post_bytes, truncate_utf8, HttpResponse};
use crate::sandbox::TrustedDir;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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
        self.save(id, updated).ok()?;
        Some(access)
    }

    pub fn save(&mut self, id: &str, token: OAuthToken) -> Result<(), String> {
        let mut updated = self.tokens.clone();
        updated.insert(id.to_string(), token);
        if let Some(dir) = &self.core_dir {
            write_json_atomic(dir, "oauth.json", &updated)?;
        }
        self.tokens = updated;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum OAuthEvent {
    DeviceCode {
        id: String,
        flow: Arc<AtomicBool>,
        code: String,
    },
    Finished {
        id: String,
        flow: Arc<AtomicBool>,
        result: Result<OAuthToken, String>,
    },
}

#[derive(Debug, Clone)]
pub struct OAuthMetadata {
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
    let trimmed_path = path.trim_end_matches('/');
    let resource_candidates = [
        (!trimmed_path.is_empty())
            .then(|| format!("{origin}/.well-known/oauth-protected-resource{path}")),
        (path != trimmed_path && !trimmed_path.is_empty())
            .then(|| format!("{origin}/.well-known/oauth-protected-resource{trimmed_path}")),
        Some(format!("{origin}/.well-known/oauth-protected-resource")),
    ];
    let mut resource_meta = resource_candidates.into_iter().flatten().find_map(|url| {
        fetch_json_object(&url, &json_headers).filter(|val| {
            val.get("authorization_servers").is_some() || val.get("resource").is_some()
        })
    });

    if resource_meta.is_none() {
        let mut probe_headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        for (k, v) in custom_headers {
            probe_headers.push((k.clone(), v.clone()));
        }
        if let Ok(resp) = post_bytes(
            mcp_url,
            &probe_headers,
            b"{}",
            OAUTH_HTTP_TIMEOUT_SECS,
            None,
        ) {
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
    let has_iss_path = !iss_path_trimmed.is_empty();
    let auth_candidates = [
        has_iss_path.then(|| {
            format!("{iss_origin}/.well-known/oauth-authorization-server{iss_path_trimmed}")
        }),
        has_iss_path
            .then(|| format!("{iss_origin}/.well-known/openid-configuration{iss_path_trimmed}")),
        has_iss_path
            .then(|| format!("{iss_origin}{iss_path_trimmed}/.well-known/openid-configuration")),
        Some(format!(
            "{iss_origin}/.well-known/oauth-authorization-server"
        )),
        Some(format!("{iss_origin}/.well-known/openid-configuration")),
        (iss_origin != origin).then(|| format!("{origin}/.well-known/oauth-authorization-server")),
    ];

    for candidate in auth_candidates.into_iter().flatten() {
        if let Some(val) = fetch_json_object(&candidate, &json_headers) {
            if let Some(token_endpoint) = str_field(&val, "token_endpoint") {
                if scopes.is_empty() {
                    scopes = extract_string_array(val.get("scopes_supported"));
                }
                return Ok(OAuthMetadata {
                    authorization_endpoint: str_field(&val, "authorization_endpoint")
                        .map(ToString::to_string),
                    token_endpoint: token_endpoint.to_string(),
                    registration_endpoint: str_field(&val, "registration_endpoint")
                        .map(ToString::to_string),
                    device_authorization_endpoint: str_field(&val, "device_authorization_endpoint")
                        .map(ToString::to_string),
                    scopes,
                });
            }
        }
    }

    Ok(OAuthMetadata {
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
            let _ = tx.send(OAuthEvent::Finished {
                id,
                flow: cancel_flag,
                result,
            });
        }
    });
    cancel
}

fn run_oauth_flow(
    id: &str,
    mcp_url: &str,
    headers: &BTreeMap<String, String>,
    cancel: &Arc<AtomicBool>,
    tx: &Sender<OAuthEvent>,
) -> Result<OAuthToken, String> {
    let meta = discover_oauth_metadata(mcp_url, headers)?;
    if cancel.load(Ordering::Relaxed) {
        return Err("授权已取消".into());
    }

    let custom_client_id = find_header_ci(headers, &["x-oauth-client-id", "client_id"]);

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
                Some((cid, None))
            } else if let Some(reg_endpoint) = &meta.registration_endpoint {
                register_dynamic_client(reg_endpoint, &redirect_uri).ok()
            } else {
                None
            };

            if let Some((client_id, client_secret)) = registered {
                return run_pkce_loopback_flow(
                    mcp_url,
                    &meta,
                    auth_endpoint,
                    listener,
                    &redirect_uri,
                    client_id,
                    client_secret,
                    cancel,
                );
            }
        }
    }

    if let Some(device_endpoint) = &meta.device_authorization_endpoint {
        let client_id = custom_client_id.ok_or_else(|| {
            "该能力要求预注册 OAuth Client ID（可在能力键值中填入 X-OAuth-Client-Id=...）"
                .to_string()
        })?;
        return run_device_code_flow(id, mcp_url, &meta, device_endpoint, &client_id, cancel, tx);
    }

    Err("该能力未提供可用的 OAuth 动态注册或设备码授权端点".into())
}

#[allow(clippy::too_many_arguments)]
fn run_pkce_loopback_flow(
    mcp_url: &str,
    meta: &OAuthMetadata,
    auth_endpoint: &str,
    listener: TcpListener,
    redirect_uri: &str,
    client_id: String,
    client_secret: Option<String>,
    cancel: &AtomicBool,
) -> Result<OAuthToken, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("授权已取消".into());
    }
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes::<32>()?);
    let challenge = pkce_s256_challenge(&verifier);
    let state = URL_SAFE_NO_PAD.encode(random_bytes::<16>()?);
    let sep = if auth_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut auth_url = format!(
        "{auth_endpoint}{sep}response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        url_encode(&client_id),
        url_encode(redirect_uri),
        url_encode(&challenge),
        url_encode(&state),
    );
    if !meta.scopes.is_empty() {
        auth_url.push_str("&scope=");
        auth_url.push_str(&url_encode(&meta.scopes.join(" ")));
    }

    let auth_url = validate_browser_url(&auth_url)?;
    open_browser(auth_url);

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
    let resp = post_form(&meta.token_endpoint, &form)
        .map_err(|err| format!("OAuth 换取 Token 请求失败: {err}"))?;
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

fn run_device_code_flow(
    id: &str,
    mcp_url: &str,
    meta: &OAuthMetadata,
    device_endpoint: &str,
    client_id: &str,
    cancel: &Arc<AtomicBool>,
    tx: &Sender<OAuthEvent>,
) -> Result<OAuthToken, String> {
    let mut form = vec![("client_id", client_id.to_string())];
    if !meta.scopes.is_empty() {
        form.push(("scope", meta.scopes.join(" ")));
    }
    let resp =
        post_form(device_endpoint, &form).map_err(|err| format!("请求 OAuth 设备码失败: {err}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "请求 OAuth 设备码失败 (HTTP {}): {}",
            resp.status,
            truncate_utf8(resp.body.trim(), 200)
        ));
    }
    let payload = serde_json::from_str::<Value>(resp.body.trim())
        .map_err(|_| "无法解析 OAuth 设备码响应".to_string())?;
    let device_code = str_field(&payload, "device_code")
        .ok_or("设备码响应缺少 device_code")?
        .to_string();
    let user_code = str_field(&payload, "user_code")
        .ok_or("设备码响应缺少 user_code")?
        .to_string();
    let verification_uri = str_field(&payload, "verification_uri_complete")
        .or_else(|| str_field(&payload, "verification_uri"))
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

    if cancel.load(Ordering::Relaxed) {
        return Err("授权已取消".into());
    }
    let verification_uri = validate_browser_url(&verification_uri)?;
    copy_to_clipboard(&user_code);
    open_browser(verification_uri);
    let _ = tx.send(OAuthEvent::DeviceCode {
        id: id.to_string(),
        flow: Arc::clone(cancel),
        code: user_code,
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

        let poll_form = [
            ("client_id", client_id.to_string()),
            ("device_code", device_code.clone()),
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            ),
        ];
        let Ok(poll_resp) = post_form(&meta.token_endpoint, &poll_form) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(poll_resp.body.trim()) else {
            continue;
        };
        if str_field(&val, "access_token").is_some() {
            return parse_token_response(
                &poll_resp.body,
                mcp_url,
                &meta.token_endpoint,
                Some(client_id.to_string()),
                None,
            );
        }
        match str_field(&val, "error") {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval_secs += 5;
                continue;
            }
            Some("expired_token") => return Err("OAuth 设备授权码已过期，请重新点击授权".into()),
            Some("access_denied") => return Err("OAuth 授权被拒绝".into()),
            Some(other) => {
                let desc = str_field(&val, "error_description").unwrap_or(other);
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
        None,
    )
    .map_err(|err| format!("OAuth 动态注册请求失败: {err:?}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("OAuth 动态注册失败 (HTTP {})", resp.status));
    }
    let val = serde_json::from_str::<Value>(&resp.body)
        .map_err(|err| format!("解析动态注册响应失败: {err}"))?;
    let client_id = str_field(&val, "client_id")
        .ok_or("动态注册响应缺少 client_id")?
        .to_string();
    let client_secret = str_field(&val, "client_secret").map(ToString::to_string);
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
                    let _ =
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
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
    let resp = post_form(token_endpoint, &form)
        .map_err(|err| format!("刷新 OAuth Token 请求失败: {err}"))?;
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
    let val = serde_json::from_str::<Value>(raw_body.trim()).map_err(|_| {
        format!(
            "无法解析 OAuth Token 响应: {}",
            truncate_utf8(raw_body, 200)
        )
    })?;
    let access_token = str_field(&val, "access_token")
        .ok_or_else(|| {
            let err = str_field(&val, "error_description")
                .or_else(|| str_field(&val, "error"))
                .unwrap_or("响应缺少 access_token");
            format!("OAuth 获取 Token 失败: {err}")
        })?
        .to_string();
    let refresh_token = str_field(&val, "refresh_token").map(ToString::to_string);
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

fn post_form(url: &str, pairs: &[(&str, String)]) -> Result<HttpResponse, String> {
    let body = encode_form(pairs);
    post_bytes(
        url,
        &[
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("Accept".to_string(), "application/json".to_string()),
        ],
        body.as_bytes(),
        OAUTH_HTTP_TIMEOUT_SECS,
        None,
    )
    .map_err(|err| format!("{err:?}"))
}

fn str_field<'a>(val: &'a Value, key: &str) -> Option<&'a str> {
    val.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn fetch_json_object(url: &str, headers: &[(String, String)]) -> Option<Value> {
    let resp = get_bytes(url, headers, OAUTH_HTTP_TIMEOUT_SECS, true).ok()?;
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
    match after_scheme.find(['/', '?', '#']) {
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

fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|err| format!("读取系统随机数失败: {err}"))?;
    Ok(bytes)
}

fn pkce_s256_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn validate_browser_url(url: &str) -> Result<&str, String> {
    if url.is_empty() || url.bytes().any(|byte| byte <= 0x20 || byte == 0x7f) {
        return Err("OAuth 授权地址包含非法字符".into());
    }
    let Some((origin, _)) = split_origin_and_path(url) else {
        return Err("OAuth 授权地址格式无效".into());
    };
    let lower = origin.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(url);
    }
    if let Some(authority) = lower.strip_prefix("http://") {
        let host = if let Some(rest) = authority.strip_prefix('[') {
            rest.split(']').next().unwrap_or("")
        } else {
            authority.split(':').next().unwrap_or("")
        };
        if matches!(host, "127.0.0.1" | "localhost" | "::1") {
            return Ok(url);
        }
    }
    Err("OAuth 授权地址必须使用 HTTPS（或本地回环 HTTP）".into())
}

#[cfg(not(test))]
fn open_browser(url: &str) {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("-u")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
fn open_browser(url: &str) {
    *TEST_BROWSER_URL.lock().unwrap() = Some(url.to_string());
}

#[cfg(test)]
static TEST_BROWSER_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

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
        assert_eq!(
            reloaded
                .access_token("notion", "https://mcp.notion.com/mcp")
                .as_deref(),
            Some("sec_123")
        );
        assert!(reloaded
            .access_token("notion", "https://evil.example.com/mcp")
            .is_none());
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
                            "registration_endpoint": format!("{base_srv}/register"),
                            "scopes_supported": ["read:mcp", "write:mcp"]
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
        *TEST_BROWSER_URL.lock().unwrap() = None;
        let _cancel = spawn_oauth_flow("notion".into(), format!("{base}/mcp"), BTreeMap::new(), tx);

        let auth_url = loop {
            if let Some(url) = TEST_BROWSER_URL.lock().unwrap().take() {
                break url;
            }
            thread::sleep(Duration::from_millis(20));
        };

        let query = auth_url.split_once('?').unwrap().1;
        let params = parse_query_params(query);
        let redirect_uri = params.get("redirect_uri").unwrap();
        let state = params.get("state").unwrap();
        let challenge = params.get("code_challenge").unwrap();
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some("read:mcp write:mcp")
        );

        let callback_url = format!(
            "{redirect_uri}?code=auth_code_777&state={}",
            url_encode(state)
        );
        let cb_resp = get_bytes(&callback_url, &[], 5, false).unwrap();
        assert_eq!(cb_resp.status, 200);

        let OAuthEvent::Finished { id, result, .. } =
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

    #[test]
    fn validate_browser_url_allows_https_and_loopback_only() {
        assert!(validate_browser_url("https://github.com/login/device").is_ok());
        assert!(validate_browser_url("http://127.0.0.1:8080/authorize?x=1").is_ok());
        assert!(validate_browser_url("http://localhost:3000/auth").is_ok());
        assert!(validate_browser_url("http://[::1]:3000/auth").is_ok());

        assert!(validate_browser_url("http://example.com/authorize").is_err());
        assert!(validate_browser_url("file:///Applications/Calculator.app").is_err());
        assert!(validate_browser_url("javascript:alert(1)").is_err());
        assert!(validate_browser_url("-a Calculator").is_err());
        assert!(validate_browser_url("https://example.com/a b").is_err());
        assert!(validate_browser_url("https://example.com/a\nb").is_err());
    }
}
