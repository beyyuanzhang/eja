use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::thread;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    Launch(String),
    Wait(String),
    Transport {
        exit_code: Option<i32>,
        message: String,
    },
    InvalidResponse(String),
}

pub fn get_bytes(
    url: &str,
    headers: &[(String, String)],
    timeout_secs: u64,
) -> Result<HttpResponse, HttpError> {
    request_bytes("GET", true, url, headers, None, timeout_secs, None)
}

pub fn post_bytes(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout_secs: u64,
) -> Result<HttpResponse, HttpError> {
    request_bytes("POST", false, url, headers, Some(body), timeout_secs, None)
}

pub fn post_stream(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout_secs: u64,
    mut on_line: impl FnMut(&str),
) -> Result<HttpResponse, HttpError> {
    request_bytes(
        "POST",
        false,
        url,
        headers,
        Some(body),
        timeout_secs,
        Some(&mut on_line),
    )
}

fn request_bytes(
    method: &str,
    follow_redirects: bool,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout_secs: u64,
    on_line: Option<&mut dyn FnMut(&str)>,
) -> Result<HttpResponse, HttpError> {
    let mut cmd = Command::new("/usr/bin/curl");
    cmd.args([
        "-sS",
        "-i",
        "-X",
        method,
        "--max-time",
        &timeout_secs.max(1).to_string(),
    ]);
    if on_line.is_some() {
        cmd.arg("-N");
    }
    if follow_redirects {
        cmd.arg("-L");
    }
    for (key, value) in headers {
        cmd.arg("-H").arg(format!("{key}: {value}"));
    }
    if body.is_some() {
        cmd.args(["--data-binary", "@-"]);
    }
    cmd.arg(url)
        .stdin(if body.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|err| HttpError::Launch(format!("启动 curl 失败: {err}")))?;
    if let (Some(mut stdin), Some(body)) = (child.stdin.take(), body) {
        let payload = body.to_vec();
        thread::spawn(move || {
            let _ = stdin.write_all(&payload);
        });
    }
    let stderr_handle = child.stderr.take().map(|mut err_pipe| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err_pipe.read_to_end(&mut buf);
            buf
        })
    });
    let mut reader = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| HttpError::Launch("无法获取 curl stdout".into()))?,
    );
    let (mut raw, mut line_buf, mut in_headers, mut status_code) =
        (String::new(), Vec::new(), true, 0u16);
    let mut on_line = on_line;
    loop {
        line_buf.clear();
        if reader
            .read_until(b'\n', &mut line_buf)
            .map_err(|err| HttpError::Wait(format!("等待 HTTP 响应失败: {err}")))?
            == 0
        {
            break;
        }
        let line = String::from_utf8_lossy(&line_buf);
        raw.push_str(&line);
        if in_headers {
            if let Some(rest) = line.strip_prefix("HTTP/") {
                status_code = rest
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            } else if (line == "\r\n" || line == "\n") && !(100..200).contains(&status_code) {
                in_headers = false;
            }
        } else if (200..300).contains(&status_code) {
            if let Some(cb) = on_line.as_deref_mut() {
                cb(line.trim_end_matches(['\r', '\n']));
            }
        }
    }
    let status = child
        .wait()
        .map_err(|err| HttpError::Wait(format!("等待 HTTP 响应失败: {err}")))?;
    if !status.success() {
        let stderr = stderr_handle.and_then(|h| h.join().ok()).unwrap_or_default();
        return Err(HttpError::Transport {
            exit_code: status.code(),
            message: String::from_utf8_lossy(&stderr).trim().to_string(),
        });
    }
    parse_http_response(&raw)
}

pub fn parse_http_response(raw: &str) -> Result<HttpResponse, HttpError> {
    let mut rest = raw;
    let mut last_headers = "";
    let mut status_code: Option<u16> = None;

    while rest.starts_with("HTTP/") {
        let Some((head, tail)) = split_once_header_end(rest) else {
            return Err(HttpError::InvalidResponse(format!(
                "无效 HTTP 响应: {}",
                truncate_utf8(rest, 240)
            )));
        };
        let code = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        last_headers = head;
        status_code = Some(code);
        rest = tail;
        if !(100..200).contains(&code)
            && !((300..400).contains(&code) && rest.starts_with("HTTP/"))
        {
            break;
        }
    }

    let Some(status) = status_code else {
        return Err(HttpError::InvalidResponse(format!(
            "缺少 HTTP 状态行: {}",
            truncate_utf8(raw.trim(), 240)
        )));
    };
    Ok(HttpResponse {
        status,
        headers: last_headers.to_string(),
        body: rest.to_string(),
    })
}

fn split_once_header_end(input: &str) -> Option<(&str, &str)> {
    [("\r\n\r\n", 4), ("\n\n", 2)]
        .into_iter()
        .filter_map(|(sep, len)| input.find(sep).map(|pos| (pos, len)))
        .min_by_key(|&(pos, _)| pos)
        .map(|(pos, len)| (&input[..pos], &input[pos + len..]))
}

pub fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    if max_bytes < 3 {
        return ".".repeat(max_bytes);
    }
    let end = input.floor_char_boundary(max_bytes - 3);
    format!("{}...", &input[..end])
}
