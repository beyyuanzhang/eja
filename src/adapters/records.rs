//! A single reader/selector path for frozen records and checkpointed files.
use super::RecordReader;
use crate::{Control, Result, Selector};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    fs,
    io::{self, BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, ChildStdout, Command, Stdio},
};

pub struct RecordFiles {
    root: PathBuf,
    checkpoints: Option<PathBuf>,
    max_bytes: usize,
}
impl RecordFiles {
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
                if rest != "context.jsonl" && rest != "aja.json" {
                    let _: u64 = id.parse().map_err(|_| "aja path needs a numeric id")?;
                    let manifest: Value = serde_json::from_slice(&fs::read(
                        super::tools::relative_file(&self.root, &format!("{id}/aja.json"))?,
                    )?)?;
                    let commit = manifest["checkpoint"]
                        .as_str()
                        .ok_or("this aja left no checkpoint")?;
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

impl RecordReader for RecordFiles {
    fn slice(&self, path: &str, selector: &Selector, control: &Control) -> Result<Value> {
        control.check()?;
        super::tools::relative_file(&self.root, path)?;
        if matches!(selector, Selector::Bytes { .. }) {
            return Err("aja files use whole, range or match".into());
        }
        if matches!(selector, Selector::Whole) {
            return super::text_view(
                self.source(path)?,
                self.max_bytes,
                json!({"path":path}),
                control,
            );
        }
        let mut reader = BufReader::new(self.source(path)?);
        let mut all = VecDeque::new();
        let mut selected = Vec::new();
        let mut hits: Vec<(usize, Vec<Value>)> = Vec::new();
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
                for (_, context) in hits
                    .iter_mut()
                    .rev()
                    .take_while(|(at, _)| total <= at.saturating_add(after))
                {
                    let size = line.to_string().len() + 1;
                    if bytes.saturating_add(size) < crate::aja::FRAME_LIMIT {
                        context.push(line.clone());
                        bytes += size;
                    } else {
                        truncated = true;
                    }
                }
                if text.contains(needle) {
                    let mut context: Vec<_> = all.iter().cloned().collect();
                    context.push(line.clone());
                    let size = serde_json::to_vec(&context)?.len() + 64;
                    if bytes.saturating_add(size) < crate::aja::FRAME_LIMIT {
                        bytes += size;
                        hits.push((total, context));
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
        let value = if matches!(selector, Selector::Range { .. }) {
            json!({"path":path,"total_lines":total,"from":from.min(total),"lines":selected,"truncated":truncated})
        } else {
            let hits: Vec<_> = hits
                .into_iter()
                .map(|(line, context)| json!({"line":line,"context":context}))
                .collect();
            json!({"path":path,"total_lines":total,"matches":hits,"truncated":truncated})
        };
        if value.to_string().len() > crate::aja::FRAME_LIMIT {
            return Err("selected record view exceeds output budget; narrow the selector".into());
        }
        Ok(value)
    }
}
