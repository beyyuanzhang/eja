//! A single reader/selector path for frozen records and checkpointed files.
use super::ArtifactReader;
use crate::{ArtifactContent, Control, Result, Selector};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    fs,
    io::{self, BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, ChildStdout, Command, Stdio},
};

pub struct FileArtifactReader {
    root: PathBuf,
    checkpoints: Option<PathBuf>,
    max_bytes: usize,
}
impl FileArtifactReader {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            checkpoints: None,
            max_bytes: crate::aja::FRAME_LIMIT,
        }
    }
    pub fn with_checkpoints(root: impl Into<PathBuf>, checkpoints: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            checkpoints: Some(checkpoints.into()),
            max_bytes: crate::aja::FRAME_LIMIT,
        }
    }
    pub fn with_limit(root: impl Into<PathBuf>, max_bytes: usize) -> Self {
        Self {
            root: root.into(),
            checkpoints: None,
            max_bytes,
        }
    }
    fn source(&self, path: &str) -> Result<Source> {
        let local = super::tools::relative_file(&self.root, path)?;
        if let Some((id, rest)) = path.split_once('/') {
            if let Some(checkpoints) = &self.checkpoints {
                if rest != "context.jsonl" && rest != "node.json" {
                    let _: u64 = id.parse().map_err(|_| "node path needs a numeric id")?;
                    let manifest: Value = serde_json::from_slice(&fs::read(
                        super::tools::relative_file(&self.root, &format!("{id}/node.json"))?,
                    )?)?;
                    let commit = manifest["checkpoint"]
                        .as_str()
                        .ok_or("this node left no checkpoint")?;
                    if commit.len() != 40 && commit.len() != 64
                        || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
                    {
                        return Err("invalid checkpoint id".into());
                    }
                    let mut child = Command::new("git")
                        .arg("--git-dir")
                        .arg(checkpoints)
                        .arg("show")
                        .arg(format!("{commit}:{rest}"))
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()?;
                    let output = child.stdout.take().ok_or("missing checkpoint reader")?;
                    return Ok(Source::Git { child, output });
                }
            }
        }
        let file = fs::File::open(local)?;
        if !file.metadata()?.is_file() {
            return Err("not a regular file".into());
        }
        Ok(Source::File(file))
    }
    fn whole(&self, path: &str) -> Result<ArtifactContent> {
        let mut bytes = Vec::new();
        self.source(path)?
            .take(self.max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        let truncated = bytes.len() > self.max_bytes;
        bytes.truncate(self.max_bytes);
        Ok(ArtifactContent {
            path: path.into(),
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
            error: None,
        })
    }
}

enum Source {
    File(fs::File),
    Git { child: Child, output: ChildStdout },
}
impl Read for Source {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.read(bytes),
            Self::Git { child, output } => {
                let count = output.read(bytes)?;
                if count == 0 && !child.wait()?.success() {
                    return Err(io::Error::other("checkpoint path does not exist"));
                }
                Ok(count)
            }
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        if let Self::Git { child, .. } = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ArtifactReader for FileArtifactReader {
    fn read(&self, path: &str) -> ArtifactContent {
        self.whole(path)
            .unwrap_or_else(|error| ArtifactContent::failed(path, error.to_string()))
    }
    fn slice(&self, path: &str, selector: &Selector) -> Result<Value> {
        let control = Control::new(
            std::time::Duration::from_secs(60),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        self.slice_controlled(path, selector, &control)
    }
    fn slice_controlled(
        &self,
        path: &str,
        selector: &Selector,
        control: &Control,
    ) -> Result<Value> {
        control.check()?;
        super::tools::relative_file(&self.root, path)?;
        if matches!(selector, Selector::Bytes { .. }) {
            return Err("node files use whole, range or match".into());
        }
        if matches!(selector, Selector::Whole) {
            let mut content = self.read(path);
            while serde_json::to_vec(&content)?.len() > crate::aja::FRAME_LIMIT {
                control.check()?;
                if content.text.is_empty() {
                    return Err("artifact metadata exceeds the view budget".into());
                }
                content.text = super::cut_chars(&content.text, content.text.len() / 2).to_string();
                content.truncated = true;
            }
            return Ok(serde_json::to_value(content)?);
        }
        let mut reader = BufReader::new(self.source(path)?);
        let mut all = VecDeque::new();
        let mut selected = Vec::new();
        let mut hits: Vec<Value> = Vec::new();
        let mut total = 0usize;
        let mut bytes = 256usize;
        let mut truncated = false;
        let (from, count) = match selector {
            Selector::Range { from, lines } => (*from, *lines),
            _ => (0, 0),
        };
        let (needle, before, after) = match selector {
            Selector::Match {
                text,
                before,
                after,
            } => (text.as_str(), *before, *after),
            _ => ("", 0, 0),
        };
        let mut pending: Vec<(usize, usize, Vec<Value>)> = Vec::new();
        loop {
            control.check()?;
            let mut raw = Vec::new();
            let read = reader
                .by_ref()
                .take((crate::aja::FRAME_LIMIT * 6 + 1) as u64)
                .read_until(b'\n', &mut raw)?;
            if read == 0 {
                break;
            }
            if raw.last() != Some(&b'\n') && raw.len() > crate::aja::FRAME_LIMIT * 6 {
                truncated = true;
                loop {
                    let rest = reader.fill_buf()?;
                    if rest.is_empty() {
                        break;
                    }
                    let end = rest.iter().position(|b| *b == b'\n');
                    let used = end.map_or(rest.len(), |end| end + 1);
                    reader.consume(used);
                    if end.is_some() {
                        break;
                    }
                    control.check()?;
                }
            }
            while raw
                .last()
                .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
            {
                raw.pop();
            }
            let text = String::from_utf8_lossy(&raw).into_owned();
            let line = json!({"line":total,"text":text});
            if matches!(selector, Selector::Range { .. })
                && total >= from
                && total < from.saturating_add(count)
            {
                let size = line.to_string().len() + 1;
                if bytes.saturating_add(size) < crate::aja::FRAME_LIMIT {
                    bytes += size;
                    selected.push(line.clone());
                } else {
                    truncated = true;
                }
            }
            if matches!(selector, Selector::Match { .. }) {
                for (_, remaining, context) in &mut pending {
                    if *remaining > 0 {
                        let size = line.to_string().len() + 1;
                        if bytes.saturating_add(size) < crate::aja::FRAME_LIMIT {
                            context.push(line.clone());
                            bytes += size;
                        } else {
                            truncated = true;
                        }
                        *remaining -= 1;
                    }
                }
                let mut i = 0;
                while i < pending.len() {
                    if pending[i].1 == 0 {
                        let (at, _, context) = pending.remove(i);
                        hits.push(json!({"line":at,"context":context}));
                    } else {
                        i += 1;
                    }
                }
                if text.contains(needle) {
                    let mut context: Vec<_> = all.iter().cloned().collect();
                    context.push(line.clone());
                    let size = serde_json::to_vec(&context)?.len() + 64;
                    if bytes.saturating_add(size) < crate::aja::FRAME_LIMIT {
                        bytes += size;
                        if after == 0 {
                            hits.push(json!({"line":total,"context":context}));
                        } else {
                            pending.push((total, after, context));
                        }
                    } else {
                        truncated = true;
                    }
                }
                // The before-window is bounded by output bytes as well as lines.
                all.push_back(line);
                while all.len() > before
                    || all.iter().map(|line| line.to_string().len()).sum::<usize>()
                        > crate::aja::FRAME_LIMIT
                {
                    all.pop_front();
                }
            }
            total += 1;
        }
        hits.extend(
            pending
                .into_iter()
                .map(|(at, _, context)| json!({"line":at,"context":context})),
        );
        hits.sort_by_key(|hit| hit["line"].as_u64());
        let value = if matches!(selector, Selector::Range { .. }) {
            json!({"path":path,"total_lines":total,"from":from.min(total),"lines":selected,"truncated":truncated})
        } else {
            json!({"path":path,"total_lines":total,"matches":hits,"truncated":truncated})
        };
        if value.to_string().len() > crate::aja::FRAME_LIMIT {
            return Err("selected record view exceeds output budget; narrow the selector".into());
        }
        Ok(value)
    }
}
