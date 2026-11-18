use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};
use std::{
    fmt, fs,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    pub fn bad(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub fn missing(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self::bad(error.to_string())
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for ApiError {
    fn from(error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self::internal(error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, axum::Json(json!({ "error": self.message }))).into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

pub fn normalize_base_url(value: &str) -> ApiResult<String> {
    let text = value.trim();
    if text.is_empty() {
        return Err(ApiError::bad("请先填写接口地址"));
    }
    let parsed = reqwest::Url::parse(text).map_err(|_| ApiError::bad("接口地址不是有效 URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad("接口地址必须是 HTTP(S) URL"));
    }
    let mut href = parsed.as_str().trim_end_matches('/').to_owned();
    for suffix in ["/chat/completions", "/models"] {
        if let Some(stripped) = href.strip_suffix(suffix) {
            href = stripped.to_owned();
            break;
        }
    }
    Ok(href)
}

pub fn provider_name(base_url: &str, fallback: &str) -> String {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(ToOwned::to_owned))
        .map(|host| host.trim_start_matches("www.").to_owned())
        .map(|host| {
            if host == "localhost" {
                "Local".to_owned()
            } else {
                host
            }
        })
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| {
            if fallback.trim().is_empty() {
                "Provider".to_owned()
            } else {
                fallback.trim().to_owned()
            }
        })
}

pub fn is_local_provider(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(ToOwned::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"))
}

pub fn unique_strings(value: Option<&Vec<String>>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    value
        .into_iter()
        .flatten()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty() && seen.insert((*item).to_owned()))
        .map(ToOwned::to_owned)
        .collect()
}

pub fn safe_file(root: &Path, relative: &str, limit: usize) -> ApiResult<(String, usize, bool)> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(ApiError::bad("path escapes its root"));
    }
    let file = root.join(relative_path);
    let base = root
        .canonicalize()
        .map_err(|_| ApiError::missing("not found"))?;
    let resolved = file
        .canonicalize()
        .map_err(|_| ApiError::missing("not found"))?;
    if !resolved.starts_with(&base) {
        return Err(ApiError::bad("path escapes its root"));
    }
    if !resolved.is_file() {
        return Err(ApiError::missing("not found"));
    }
    let mut bytes = fs::read(&resolved)?;
    let total = bytes.len();
    let truncated = total > limit;
    bytes.truncate(limit);
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        total,
        truncated,
    ))
}

pub fn write_private_json(path: &Path, value: &Value) -> ApiResult<()> {
    let body = format!("{}\n", serde_json::to_string_pretty(value)?);
    write_private_bytes(path, body.as_bytes())
}

pub fn write_private_bytes(path: &Path, bytes: &[u8]) -> ApiResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temp = PathBuf::from(format!(
        "{}.{}.{}.{}.tmp",
        path.display(),
        std::process::id(),
        nonce,
        sequence
    ));
    fs::write(&temp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&temp, path)?;
    Ok(())
}

pub fn clip(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        value.to_owned()
    } else {
        let mut end = limit.min(value.len());
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n[truncated]", &value[..end])
    }
}
