use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

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
    follow_redirects: bool,
) -> Result<HttpResponse, HttpError> {
    request_bytes(
        "GET",
        follow_redirects,
        url,
        headers,
        None,
        timeout_secs,
        None,
        None,
    )
}

pub fn post_bytes(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout_secs: u64,
    cancel: Option<&AtomicBool>,
) -> Result<HttpResponse, HttpError> {
    request_bytes(
        "POST",
        false,
        url,
        headers,
        Some(body),
        timeout_secs,
        None,
        cancel,
    )
}

pub fn post_stream(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout_secs: u64,
    cancel: Option<&AtomicBool>,
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
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
fn request_bytes(
    method: &str,
    follow_redirects: bool,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout_secs: u64,
    on_line: Option<&mut dyn FnMut(&str)>,
    cancel: Option<&AtomicBool>,
) -> Result<HttpResponse, HttpError> {
    let config = format_curl_config(url, headers)?;
    let (config_read, config_write) = create_cloexec_pipe()
        .map_err(|err| HttpError::Launch(format!("创建 curl 配置管道失败: {err}")))?;
    let config_fd = config_read.as_raw_fd();
    let config_path = if cfg!(target_os = "linux") {
        format!("/proc/self/fd/{config_fd}")
    } else {
        format!("/dev/fd/{config_fd}")
    };

    let mut cmd = Command::new("/usr/bin/curl");
    cmd.args([
        "-q",
        "-sS",
        "-i",
        "--suppress-connect-headers",
        "--globoff",
        "--proto",
        "=http,https",
        "-X",
        method,
        "--max-time",
        &timeout_secs.max(1).to_string(),
        "-K",
        &config_path,
    ]);
    if on_line.is_some() {
        cmd.arg("-N");
    }
    if follow_redirects {
        cmd.arg("-L");
    }
    if body.is_some() {
        cmd.args(["--data-binary", "@-"]);
    }
    cmd.stdin(if body.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    unsafe {
        cmd.pre_exec(move || {
            if libc::fcntl(config_fd, libc::F_SETFD, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| HttpError::Launch(format!("启动 curl 失败: {err}")))?;
    drop(config_read);
    let config_handle = thread::spawn(move || {
        let mut file = File::from(config_write);
        let _ = file.write_all(config.as_bytes());
    });
    let stdin_handle = match (child.stdin.take(), body) {
        (Some(mut stdin), Some(body)) => {
            let payload = body.to_vec();
            Some(thread::spawn(move || {
                let _ = stdin.write_all(&payload);
            }))
        }
        _ => None,
    };
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
    let pid = child.id() as libc::pid_t;
    let finished = AtomicBool::new(false);
    let raw = thread::scope(|scope| {
        if let Some(cancel) = cancel {
            let finished = &finished;
            scope.spawn(move || {
                while !finished.load(Ordering::Relaxed) {
                    if cancel.load(Ordering::Relaxed) {
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                        return;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
        }
        let raw = read_response(&mut reader, on_line);
        finished.store(true, Ordering::Relaxed);
        raw
    })?;
    let status = child
        .wait()
        .map_err(|err| HttpError::Wait(format!("等待 HTTP 响应失败: {err}")))?;
    let _ = config_handle.join();
    if let Some(handle) = stdin_handle {
        let _ = handle.join();
    }
    let stderr = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    if !status.success() {
        return Err(HttpError::Transport {
            exit_code: status.code(),
            message: String::from_utf8_lossy(&stderr).trim().to_string(),
        });
    }
    parse_http_response(&raw)
}

fn read_response(
    reader: &mut impl BufRead,
    mut on_line: Option<&mut dyn FnMut(&str)>,
) -> Result<String, HttpError> {
    let (mut raw, mut line_buf, mut in_headers, mut status_code, mut is_sse) =
        (String::new(), Vec::new(), true, 0u16, false);
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
        if in_headers {
            raw.push_str(&line);
            if let Some(rest) = line.strip_prefix("HTTP/") {
                status_code = rest
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                is_sse = false;
            } else if line.to_ascii_lowercase().starts_with("content-type:")
                && line.to_ascii_lowercase().contains("text/event-stream")
            {
                is_sse = true;
            } else if (line == "\r\n" || line == "\n") && !(100..200).contains(&status_code) {
                in_headers = false;
            }
        } else if (200..300).contains(&status_code) {
            if let Some(cb) = on_line.as_deref_mut() {
                cb(line.trim_end_matches(['\r', '\n']));
                if !is_sse {
                    raw.push_str(&line);
                }
            } else {
                raw.push_str(&line);
            }
        } else {
            raw.push_str(&line);
        }
    }
    Ok(raw)
}

fn format_curl_config(url: &str, headers: &[(String, String)]) -> Result<String, HttpError> {
    if url.is_empty() || url.contains(['\r', '\n', '\0']) {
        return Err(HttpError::Launch("无效的请求 URL".into()));
    }
    let escape = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
    let mut out = format!("url = \"{}\"\n", escape(url));
    for (key, value) in headers {
        if key.is_empty()
            || key.contains(['\r', '\n', ':', '\0'])
            || value.contains(['\r', '\n', '\0'])
        {
            return Err(HttpError::Launch("无效的 HTTP 请求头".into()));
        }
        out.push_str(&format!(
            "header = \"{}: {}\"\n",
            escape(key),
            escape(value)
        ));
    }
    Ok(out)
}

fn create_cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    for fd in [read_fd.as_raw_fd(), write_fd.as_raw_fd()] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read_fd, write_fd))
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
        if !(100..200).contains(&code) && !((300..400).contains(&code) && rest.starts_with("HTTP/"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn curl_config_escapes_quotes_and_rejects_control_characters() {
        let cfg = format_curl_config(
            "https://example.com/v1/chat?a=\"1\"\\2",
            &[
                ("Authorization".into(), "Bearer sk-\"test\"\\123".into()),
                ("X-Empty".into(), String::new()),
            ],
        )
        .unwrap();
        assert_eq!(
            cfg,
            "url = \"https://example.com/v1/chat?a=\\\"1\\\"\\\\2\"\n\
             header = \"Authorization: Bearer sk-\\\"test\\\"\\\\123\"\n\
             header = \"X-Empty: \"\n"
        );
        assert!(format_curl_config("https://example.com\nfoo", &[]).is_err());
        assert!(format_curl_config("https://example.com", &[("".into(), "v".into())]).is_err());
        assert!(format_curl_config(
            "https://example.com",
            &[("X-Key".into(), "bad\r\nheader".into())]
        )
        .is_err());
    }

    #[test]
    fn post_stream_passes_headers_via_config_pipe_and_streams_body_lines() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let count = stream.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..count]).into_owned();
            let body = "data: {\"chunk\":1}\n\ndata: [DONE]\n";
            let reply = format!(
                "HTTP/1.1 100 Continue\r\n\r\n\
                 HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            req
        });

        let mut lines = Vec::new();
        let resp = post_stream(
            &format!("http://127.0.0.1:{port}/stream"),
            &[
                ("Authorization".into(), "Bearer sec-\"quoted\"\\tok".into()),
                ("Content-Type".into(), "application/json".into()),
            ],
            br#"{"ping":true}"#,
            5,
            None,
            |line| lines.push(line.to_string()),
        )
        .unwrap();

        let raw_req = server.join().unwrap();
        assert!(raw_req.contains("Authorization: Bearer sec-\"quoted\"\\tok\r\n"));
        assert!(raw_req.ends_with(r#"{"ping":true}"#));
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_empty());
        assert_eq!(lines, vec!["data: {\"chunk\":1}", "", "data: [DONE]"]);
    }

    #[test]
    fn post_stream_stops_a_stalled_response_when_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {}\n\n",
            );
            thread::sleep(Duration::from_secs(3));
        });
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let result = post_stream(
            &format!("http://127.0.0.1:{port}/stream"),
            &[],
            b"{}",
            30,
            Some(&cancel),
            |_| cancel.store(true, Ordering::Relaxed),
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }
}
