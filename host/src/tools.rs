//! Compounding user-space deterministic tool library (`<data_dir>/tools/<name>/`).
//! Tools must pass their own deterministic `check` script inside Seatbelt before being
//! sealed (by SHA-256) in kernel-protected `host_meta` and exposed to the model.
//! Verified scripts are executed directly from verified in-memory bytes via `/bin/sh -c`
//! to eliminate TOCTOU races between digest verification and execution.
use crate::http::truncate_utf8;
use crate::sandbox::{kill_process_tree, Sandbox, TrustedDir};
use eja_core::{Eja, Outcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

const META_TOOLS: &str = "host.tools";
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 30;
const MAX_SCRIPT_BYTES: usize = 128 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolManifest {
    pub name: String,
    pub description: String,
    pub schema: Value,
    #[serde(default = "default_entrypoint")]
    pub entrypoint: String,
    #[serde(default = "default_check")]
    pub check: String,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_entrypoint() -> String {
    "run.sh".into()
}

fn default_check() -> String {
    "check.sh".into()
}

fn default_timeout_seconds() -> u64 {
    DEFAULT_TOOL_TIMEOUT_SECS
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SealedTool {
    pub capability: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub entrypoint: String,
    pub check: String,
    pub timeout_seconds: u64,
    pub digest: String,
    #[serde(default)]
    pub observed_count: u64,
    #[serde(default)]
    pub failed_count: u64,
    #[serde(default)]
    pub unknown_count: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ToolRegistryMeta {
    #[serde(default)]
    tools: BTreeMap<String, SealedTool>,
}

pub enum ScriptRunOutcome {
    Ok(String),
    NonZero { code: i32, output: String },
    Unknown(String),
}

fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn load_meta(eja: &Eja) -> ToolRegistryMeta {
    eja.get_meta(META_TOOLS)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn update_meta<T>(
    eja: &Eja,
    f: impl FnOnce(&mut ToolRegistryMeta) -> eja_core::Result<T>,
) -> eja_core::Result<T> {
    eja.transaction(|tx| {
        let mut reg = tx
            .get_meta(META_TOOLS)?
            .map(|raw| serde_json::from_str(&raw))
            .transpose()?
            .unwrap_or_default();
        let out = f(&mut reg)?;
        tx.set_meta(META_TOOLS, &serde_json::to_string(&reg)?)?;
        Ok(out)
    })
}

struct LoadedToolFiles {
    dir: TrustedDir,
    manifest: ToolManifest,
    entrypoint_script: String,
    check_script: String,
    digest: String,
}

fn validate_script_bytes(label: &str, bytes: &[u8]) -> Result<String, String> {
    if bytes.is_empty() {
        return Err(format!("{label} 不能为空"));
    }
    if bytes.len() > MAX_SCRIPT_BYTES {
        return Err(format!(
            "{label} 超过大小上限 ({} bytes > {MAX_SCRIPT_BYTES} bytes)",
            bytes.len()
        ));
    }
    if bytes.contains(&0) {
        return Err(format!("{label} 不能包含 NUL 字节"));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| format!("{label} 必须是有效 UTF-8 文本脚本"))?;
    if text.trim().is_empty() {
        return Err(format!("{label} 不能为空白脚本"));
    }
    Ok(text.to_string())
}

fn load_tool_files(data_dir: &TrustedDir, name: &str) -> Result<LoadedToolFiles, String> {
    if !is_valid_tool_name(name) {
        return Err("工具名仅支持小写字母、数字、下划线或连字符（1-40字符）".into());
    }
    let tools_dir = data_dir
        .open_subdir("tools")
        .map_err(|err| format!("打开 tools/ 目录失败（拒绝符号链接）: {err}"))?;
    let dir = tools_dir
        .open_subdir(name)
        .map_err(|err| format!("打开工具目录 tools/{name} 失败: {err}"))?;
    let manifest_bytes = dir
        .read_file("manifest.json")
        .map_err(|err| format!("读取 manifest.json 失败: {err}"))?;
    let manifest: ToolManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|err| format!("解析 manifest.json 失败: {err}"))?;
    if manifest.name != name {
        return Err(format!(
            "manifest.name ({}) 与目录名 ({name}) 不一致",
            manifest.name
        ));
    }
    if manifest.description.trim().is_empty() {
        return Err("manifest.description 不能为空".into());
    }
    if manifest.schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("manifest.schema.type 必须为 \"object\"".into());
    }
    let entrypoint_bytes = dir
        .read_file(&manifest.entrypoint)
        .map_err(|err| format!("读取入口文件 {} 失败: {err}", manifest.entrypoint))?;
    let check_bytes = dir
        .read_file(&manifest.check)
        .map_err(|err| format!("读取自检脚本 {} 失败: {err}", manifest.check))?;
    let entrypoint_script = validate_script_bytes(&manifest.entrypoint, &entrypoint_bytes)?;
    let check_script = validate_script_bytes(&manifest.check, &check_bytes)?;

    let mut hasher = Sha256::new();
    hasher.update((manifest_bytes.len() as u64).to_le_bytes());
    hasher.update(&manifest_bytes);
    hasher.update((entrypoint_bytes.len() as u64).to_le_bytes());
    hasher.update(&entrypoint_bytes);
    hasher.update((check_bytes.len() as u64).to_le_bytes());
    hasher.update(&check_bytes);
    let digest = format!("{:x}", hasher.finalize());
    Ok(LoadedToolFiles {
        dir,
        manifest,
        entrypoint_script,
        check_script,
        digest,
    })
}

pub fn normalize_tool_capability(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let name = trimmed.strip_prefix("tool.").unwrap_or(trimmed);
    if !is_valid_tool_name(name) {
        return Err("工具名仅支持小写字母、数字、下划线或连字符（1-40字符）".into());
    }
    Ok(format!("tool.{name}"))
}

pub fn promote_tool(data_dir: &TrustedDir, eja: &Eja, sandbox: &Sandbox, name: &str) -> Outcome {
    let loaded = match load_tool_files(data_dir, name) {
        Ok(files) => files,
        Err(msg) => return Outcome::Failed(msg),
    };
    let check_output = match run_script_bytes(
        sandbox,
        &loaded.check_script,
        &loaded.manifest.check,
        loaded.dir.path(),
        b"",
        CHECK_TIMEOUT,
        MAX_TOOL_OUTPUT_BYTES,
    ) {
        ScriptRunOutcome::Ok(out) => out,
        ScriptRunOutcome::NonZero { code, output } => {
            return Outcome::Failed(format!("自检脚本未通过 (exit {code}): {output}"));
        }
        ScriptRunOutcome::Unknown(msg) => {
            return Outcome::Unknown(format!("自检脚本执行异常: {msg}"));
        }
    };

    // Re-hash after check completes to prevent a self-mutating check script from altering sealed bytes.
    let rechecked = match load_tool_files(data_dir, name) {
        Ok(files) => files,
        Err(msg) => return Outcome::Failed(format!("自检后复核工具文件失败: {msg}")),
    };
    if rechecked.digest != loaded.digest {
        return Outcome::Failed("自检过程中工具文件发生变化，拒绝封签".into());
    }

    let capability = format!("tool.{}", loaded.manifest.name);
    let sealed = SealedTool {
        capability: capability.clone(),
        name: loaded.manifest.name.clone(),
        description: loaded.manifest.description.trim().to_string(),
        schema: loaded.manifest.schema.clone(),
        entrypoint: loaded.manifest.entrypoint.clone(),
        check: loaded.manifest.check.clone(),
        timeout_seconds: loaded.manifest.timeout_seconds.clamp(1, 120),
        digest: loaded.digest.clone(),
        observed_count: 0,
        failed_count: 0,
        unknown_count: 0,
    };
    match update_meta(eja, |reg| {
        reg.tools.insert(capability.clone(), sealed);
        Ok(())
    }) {
        Ok(()) => Outcome::Observed(format!(
            "promoted {capability} (sha256:{}; check: {})",
            &loaded.digest[..12],
            truncate_utf8(
                if check_output.is_empty() {
                    "ok"
                } else {
                    &check_output
                },
                256
            )
        )),
        Err(err) => Outcome::Failed(format!("保存工具封签失败: {err}")),
    }
}

/// Return only tools whose on-disk files still match their kernel-sealed SHA-256 digest.
pub fn active_verified_tools(data_dir: &TrustedDir, eja: &Eja) -> Vec<SealedTool> {
    let reg = load_meta(eja);
    let mut out = Vec::new();
    for sealed in reg.tools.into_values() {
        if let Ok(files) = load_tool_files(data_dir, &sealed.name) {
            if files.digest == sealed.digest {
                out.push(sealed);
            }
        }
    }
    out
}

fn verify_sealed_tool(
    data_dir: &TrustedDir,
    eja: &Eja,
    capability: &str,
) -> Result<(SealedTool, LoadedToolFiles), String> {
    let reg = load_meta(eja);
    let Some(sealed) = reg.tools.get(capability).cloned() else {
        return Err(format!("工具未通过验证注册: {capability}"));
    };
    let files = load_tool_files(data_dir, &sealed.name)
        .map_err(|msg| format!("工具完整性检查失败: {msg}"))?;
    if files.digest != sealed.digest {
        return Err(format!(
            "工具 {capability} 已被修改（校验和不匹配），请重新运行 promote_tool 验证"
        ));
    }
    Ok((sealed, files))
}

pub fn ensure_verified_tool_ready(
    data_dir: &TrustedDir,
    eja: &Eja,
    capability: &str,
) -> Result<SealedTool, String> {
    verify_sealed_tool(data_dir, eja, capability).map(|(sealed, _)| sealed)
}

pub fn run_verified_tool_probe(
    data_dir: &TrustedDir,
    eja: &Eja,
    sandbox: &Sandbox,
    capability: &str,
    input: &Value,
    timeout: Duration,
    max_output_bytes: usize,
) -> ScriptRunOutcome {
    let (sealed, files) = match verify_sealed_tool(data_dir, eja, capability) {
        Ok(pair) => pair,
        Err(msg) => return ScriptRunOutcome::Unknown(msg),
    };
    let stdin_bytes = serde_json::to_vec(input).unwrap_or_else(|_| b"{}".to_vec());
    let effective_timeout = timeout.min(Duration::from_secs(sealed.timeout_seconds.clamp(1, 120)));
    run_script_bytes(
        sandbox,
        &files.entrypoint_script,
        &sealed.entrypoint,
        data_dir.path(),
        &stdin_bytes,
        effective_timeout,
        max_output_bytes,
    )
}

pub fn execute_verified_tool(
    data_dir: &TrustedDir,
    eja: &Eja,
    sandbox: &Sandbox,
    capability: &str,
    input: &Value,
) -> Outcome {
    let (sealed, files) = match verify_sealed_tool(data_dir, eja, capability) {
        Ok(pair) => pair,
        Err(msg) => return Outcome::Failed(msg),
    };
    let stdin_bytes = serde_json::to_vec(input).unwrap_or_else(|_| b"{}".to_vec());
    let outcome = match run_script_bytes(
        sandbox,
        &files.entrypoint_script,
        &sealed.entrypoint,
        data_dir.path(),
        &stdin_bytes,
        Duration::from_secs(sealed.timeout_seconds.clamp(1, 120)),
        MAX_TOOL_OUTPUT_BYTES,
    ) {
        ScriptRunOutcome::Ok(out) => {
            Outcome::Observed(if out.is_empty() { "(no output)".into() } else { out })
        }
        ScriptRunOutcome::NonZero { code, output } => {
            let detail = if output.is_empty() {
                "(no output)".to_string()
            } else {
                output
            };
            Outcome::Failed(format!("exit {code}: {detail}"))
        }
        ScriptRunOutcome::Unknown(msg) => Outcome::Unknown(msg),
    };
    let _ = update_meta(eja, |reg| {
        if let Some(item) = reg.tools.get_mut(capability) {
            match &outcome {
                Outcome::Observed(_) => item.observed_count = item.observed_count.saturating_add(1),
                Outcome::Failed(_) => item.failed_count = item.failed_count.saturating_add(1),
                Outcome::Unknown(_) => item.unknown_count = item.unknown_count.saturating_add(1),
            }
        }
        Ok(())
    });
    outcome
}

fn run_script_bytes(
    sandbox: &Sandbox,
    script_body: &str,
    script_name: &str,
    cwd: &Path,
    stdin_payload: &[u8],
    timeout: Duration,
    max_output_bytes: usize,
) -> ScriptRunOutcome {
    let mut child = match sandbox
        .command("/bin/sh", ["-c", script_body, script_name])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return ScriptRunOutcome::Unknown(format!("启动工具脚本失败: {err}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_payload);
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_handle = thread::spawn(move || read_bounded(stdout, max_output_bytes));
    let err_handle = thread::spawn(move || read_bounded(stderr, max_output_bytes));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) if out_handle.is_finished() && err_handle.is_finished() => break Some(st),
            Ok(_) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            _ => {
                kill_process_tree(&mut child);
                break None;
            }
        }
    };
    let out = out_handle.join().ok().and_then(Result::ok).unwrap_or_default();
    let err = err_handle.join().ok().and_then(Result::ok).unwrap_or_default();
    let combined = match (out.trim().is_empty(), err.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => out.trim().to_string(),
        (true, false) => err.trim().to_string(),
        (false, false) => format!("{}\n[stderr]\n{}", out.trim(), err.trim()),
    };
    let bounded = truncate_utf8(&combined, max_output_bytes);
    match status {
        Some(st) if st.success() => ScriptRunOutcome::Ok(bounded),
        Some(st) => ScriptRunOutcome::NonZero {
            code: st.code().unwrap_or(-1),
            output: bounded,
        },
        None => ScriptRunOutcome::Unknown(format!(
            "工具超时 ({}s): {}",
            timeout.as_secs(),
            bounded
        )),
    }
}

fn read_bounded(mut reader: impl Read, max_output_bytes: usize) -> io::Result<String> {
    let mut buf = Vec::new();
    reader
        .by_ref()
        .take(max_output_bytes as u64 + 1)
        .read_to_end(&mut buf)?;
    let _ = io::copy(&mut reader, &mut io::sink());
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
