use std::ffi::{CString, OsStr};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[cfg(target_os = "macos")]
use libc::{
    mkdirat, openat, renameat, unlinkat, O_CLOEXEC, O_CREAT, O_DIRECTORY, O_EXCL, O_NOFOLLOW,
    O_NONBLOCK, O_RDONLY, O_WRONLY,
};
#[cfg(target_os = "macos")]
use std::collections::BTreeSet;
#[cfg(target_os = "macos")]
use std::ffi::OsString;
#[cfg(target_os = "macos")]
use std::fs::File;
#[cfg(target_os = "macos")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "macos")]
use std::os::unix::io::{FromRawFd, OwnedFd};

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn validate_single_name(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid file name",
        ));
    }
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"))
}

#[cfg(target_os = "macos")]
fn openat_file(dirfd: i32, name: &CString, flags: i32, mode: Option<u32>) -> io::Result<File> {
    let fd = unsafe {
        match mode {
            Some(file_mode) => openat(dirfd, name.as_ptr(), flags, file_mode),
            None => openat(dirfd, name.as_ptr(), flags),
        }
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(File::from(owned))
}

#[cfg(not(target_os = "macos"))]
fn require_real_dir(path: &Path, message: &str) -> io::Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, message));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct TrustedDir {
    path: PathBuf,
    #[cfg(target_os = "macos")]
    dir: Arc<File>,
}

impl TrustedDir {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let path = dir.into();
        #[cfg(target_os = "macos")]
        {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
                .open(&path)?;
            Ok(Self {
                path,
                dir: Arc::new(file),
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            require_real_dir(&path, "root is not a real directory")?;
            Ok(Self { path })
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_check_file(&self, name: &str, limit: usize) -> io::Result<Vec<u8>> {
        let name = validate_single_name(name)?;
        #[cfg(target_os = "macos")]
        {
            let file = openat_file(
                self.dir.as_raw_fd(),
                &name,
                O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK,
                None,
            )?;
            if !file.metadata()?.is_file() {
                return Err(io::Error::other("checker requires a regular file"));
            }
            let mut bytes = Vec::new();
            file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > limit {
                return Err(io::Error::other("checker file exceeds size limit"));
            }
            Ok(bytes)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (name, limit);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file checks require macOS",
            ))
        }
    }

    pub fn read_file(&self, name: &str) -> io::Result<Vec<u8>> {
        let c_name = validate_single_name(name)?;
        #[cfg(target_os = "macos")]
        {
            let mut file = openat_file(
                self.dir.as_raw_fd(),
                &c_name,
                O_RDONLY | O_NOFOLLOW | O_CLOEXEC,
                None,
            )?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = c_name;
            require_real_dir(&self.path, "root is not a real directory")?;
            let path = self.path.join(name);
            if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to follow symlink",
                ));
            }
            fs::read(path)
        }
    }

    pub fn write_new_file_in_subdir(
        &self,
        subdir: &str,
        name: &str,
        mut input: impl io::Read,
    ) -> io::Result<PathBuf> {
        let c_subdir = validate_single_name(subdir)?;
        let c_name = validate_single_name(name)?;
        #[cfg(target_os = "macos")]
        {
            let dirfd = self.dir.as_raw_fd();
            if unsafe { mkdirat(dirfd, c_subdir.as_ptr(), 0o755) } < 0 {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::AlreadyExists {
                    return Err(err);
                }
            }
            let sub_file = openat_file(
                dirfd,
                &c_subdir,
                O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
                None,
            )?;
            let mut file = openat_file(
                sub_file.as_raw_fd(),
                &c_name,
                O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC,
                Some(0o644),
            )?;
            io::copy(&mut input, &mut file)?;
            Ok(self.path.join(subdir).join(name))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (c_subdir, c_name);
            require_real_dir(&self.path, "root is not a real directory")?;
            let subdir_path = self.path.join(subdir);
            match fs::create_dir(&subdir_path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
            require_real_dir(&subdir_path, "subdir is not a real directory")?;
            let target = subdir_path.join(name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            io::copy(&mut input, &mut file)?;
            Ok(target)
        }
    }

    pub fn write_file_atomic(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let c_name = validate_single_name(name)?;
        let pid = std::process::id();
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_name = format!(".{name}.{pid}.{seq}.tmp");
        let c_tmp = validate_single_name(&tmp_name)?;
        #[cfg(target_os = "macos")]
        {
            let dirfd = self.dir.as_raw_fd();
            let mut file = openat_file(
                dirfd,
                &c_tmp,
                O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC,
                Some(0o600),
            )?;
            if let Err(err) = file.write_all(bytes) {
                drop(file);
                unsafe {
                    unlinkat(dirfd, c_tmp.as_ptr(), 0);
                }
                return Err(err);
            }
            drop(file);
            if unsafe { renameat(dirfd, c_tmp.as_ptr(), dirfd, c_name.as_ptr()) } < 0 {
                let err = io::Error::last_os_error();
                unsafe {
                    unlinkat(dirfd, c_tmp.as_ptr(), 0);
                }
                return Err(err);
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (c_name, c_tmp);
            require_real_dir(&self.path, "root is not a real directory")?;
            let tmp_path = self.path.join(&tmp_name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            if let Err(err) = file.write_all(bytes) {
                drop(file);
                let _ = fs::remove_file(&tmp_path);
                return Err(err);
            }
            drop(file);
            if let Err(err) = fs::rename(&tmp_path, self.path.join(name)) {
                let _ = fs::remove_file(&tmp_path);
                return Err(err);
            }
            Ok(())
        }
    }
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    #[cfg(target_os = "macos")]
    profile: String,
    #[cfg(target_os = "macos")]
    params: Vec<OsString>,
    cancel: Arc<AtomicBool>,
}

impl Sandbox {
    #[cfg(target_os = "macos")]
    pub fn new(protected_dir: &Path) -> Result<Self, String> {
        let host_exe = std::env::current_exe().map_err(|err| format!("定位宿主程序失败: {err}"))?;
        Self::with_host_exe(protected_dir, &host_exe)
    }

    #[cfg(target_os = "macos")]
    fn with_host_exe(protected_dir: &Path, host_exe: &Path) -> Result<Self, String> {
        let protected =
            fs::canonicalize(protected_dir).map_err(|err| format!("规范化核心目录失败: {err}"))?;
        let host_exe =
            fs::canonicalize(host_exe).map_err(|err| format!("定位宿主程序失败: {err}"))?;
        let app_bundle = app_bundle_root(&host_exe);
        let ancestors = collect_ancestors(&[protected.as_path(), host_exe.as_path()]);
        let (profile, params) =
            build_seatbelt_policy(&protected, &host_exe, app_bundle.as_deref(), &ancestors);
        Ok(Self {
            profile,
            params,
            cancel: Arc::default(),
        })
    }

    #[cfg(not(target_os = "macos"))]
    pub fn new(_protected_dir: &Path) -> Result<Self, String> {
        Ok(Self {
            cancel: Arc::default(),
        })
    }

    /// Shared stop switch for work started under this sandbox; clones observe the same flag.
    pub fn cancel_flag(&self) -> &Arc<AtomicBool> {
        &self.cancel
    }

    pub fn command<I, S>(&self, program: impl AsRef<OsStr>, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        #[cfg(target_os = "macos")]
        let mut cmd = {
            let mut cmd = Command::new(SANDBOX_EXEC);
            cmd.arg("-p").arg(&self.profile);
            cmd.args(&self.params);
            cmd.arg("--").arg(program).args(args);
            cmd
        };
        #[cfg(not(target_os = "macos"))]
        let mut cmd = {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd
        };
        cmd.stdin(Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0);
        cmd
    }
}

pub fn kill_process_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        if pid > 0 {
            unsafe {
                let _ = libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub fn write_process_input(
    stdin: &mut ChildStdin,
    mut data: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    let fd = stdin.as_raw_fd();
    make_nonblocking(fd)?;
    while !data.is_empty() {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match stdin.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(size) => data = &data[size..],
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                wait_for_pipe(fd, libc::POLLOUT, deadline)?;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

pub struct ProcessOutput<R> {
    pipe: R,
    deadline: Instant,
}

impl<R: Read + AsRawFd> ProcessOutput<R> {
    pub fn new(pipe: R, deadline: Instant) -> io::Result<Self> {
        make_nonblocking(pipe.as_raw_fd())?;
        Ok(Self { pipe, deadline })
    }
}

impl<R: Read + AsRawFd> Read for ProcessOutput<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            if Instant::now() >= self.deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.pipe.read(buffer) {
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    wait_for_pipe(self.pipe.as_raw_fd(), libc::POLLIN, self.deadline)?;
                }
                result => return result,
            }
        }
    }
}

fn make_nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wait_for_pipe(fd: i32, events: i16, deadline: Instant) -> io::Result<()> {
    let mut poll_fd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let millis = remaining
            .as_nanos()
            .div_ceil(1_000_000)
            .min(i32::MAX as u128) as i32;
        match unsafe { libc::poll(&mut poll_fd, 1, millis) } {
            value if value > 0 => return Ok(()),
            -1 => {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
            _ => {}
        }
    }
}

#[cfg(target_os = "macos")]
fn app_bundle_root(host_exe: &Path) -> Option<PathBuf> {
    let macos_dir = host_exe.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let contents_dir = macos_dir.parent()?;
    if contents_dir.file_name()? != "Contents" {
        return None;
    }
    let bundle = contents_dir.parent()?;
    if bundle.extension()? != "app" {
        return None;
    }
    Some(bundle.to_path_buf())
}

#[cfg(target_os = "macos")]
fn collect_ancestors(roots: &[&Path]) -> BTreeSet<PathBuf> {
    let mut unique = BTreeSet::new();
    for root in roots {
        for path in root.ancestors().take_while(|p| p.parent().is_some()) {
            unique.insert(path.to_path_buf());
        }
    }
    unique
}

#[cfg(target_os = "macos")]
fn build_seatbelt_policy(
    protected_dir: &Path,
    host_exe: &Path,
    app_bundle: Option<&Path>,
    ancestors: &BTreeSet<PathBuf>,
) -> (String, Vec<OsString>) {
    let mut profile = String::from(
        "(version 1)\n\
         (allow default)\n\
         (deny signal)\n\
         (allow signal (target same-sandbox))\n\
         (deny process-info-pidinfo)\n\
         (allow process-info-pidinfo (target same-sandbox))\n\
         (deny file-read* file-write* (subpath (param \"PROTECTED_DIR\")))\n",
    );
    let mut params = vec![define_param("PROTECTED_DIR", protected_dir)];
    if let Some(bundle) = app_bundle {
        profile.push_str("(deny file-write* (subpath (param \"APP_BUNDLE\")))\n");
        params.push(define_param("APP_BUNDLE", bundle));
    } else {
        profile.push_str("(deny file-write* (literal (param \"HOST_EXE\")))\n");
        params.push(define_param("HOST_EXE", host_exe));
    }
    for (index, ancestor) in ancestors.iter().enumerate() {
        let key = format!("ANCESTOR_{index}");
        profile.push_str(&format!(
            "(deny file-write-unlink (literal (param \"{key}\")))\n"
        ));
        params.push(define_param(&key, ancestor));
    }
    (profile, params)
}

#[cfg(target_os = "macos")]
fn define_param(key: &str, path: &Path) -> OsString {
    let mut arg = OsString::from(format!("-D{key}="));
    arg.push(path.as_os_str());
    arg
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn policy_includes_protected_dir_host_exe_and_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();

        let sandbox = Sandbox::new(&core_dir).unwrap();
        assert!(sandbox
            .profile
            .contains("(deny file-read* file-write* (subpath (param \"PROTECTED_DIR\")))"));
        assert!(sandbox
            .profile
            .contains("(deny file-write* (literal (param \"HOST_EXE\")))"));
        assert!(sandbox
            .profile
            .contains("(deny file-write-unlink (literal (param \"ANCESTOR_0\")))"));

        let canonical_core = fs::canonicalize(&core_dir).unwrap();
        let canonical_parent = canonical_core.parent().unwrap();
        assert!(sandbox
            .params
            .iter()
            .any(|p| p == &define_param("PROTECTED_DIR", &canonical_core)));
        assert!(sandbox.params.iter().any(|p| {
            p.to_string_lossy()
                .ends_with(&format!("={}", canonical_parent.display()))
        }));
    }

    #[test]
    fn seatbelt_blocks_core_access_rename_and_parent_signal_while_allowing_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("eja");
        let core_dir = dir.path().join("ai.eja.kernel");
        fs::create_dir(&data_dir).unwrap();
        fs::create_dir_all(&core_dir).unwrap();
        let db = core_dir.join("subject.db");
        fs::write(&db, "secret-journal").unwrap();
        let core_config = core_dir.join("config.json");
        fs::write(&core_config, "{\"api_key\":\"secret-key\"}").unwrap();
        let capabilities = data_dir.join("capabilities.json");
        fs::write(&capabilities, "[]").unwrap();
        let fake_exe = dir.path().join("eja-host");
        fs::write(&fake_exe, "host-binary").unwrap();

        let sandbox = Sandbox::with_host_exe(&core_dir, &fake_exe).unwrap();

        let probe = sandbox
            .command("/bin/sh", ["-c", "echo ok"])
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "sandbox-exec failed to run: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "ok");

        let edit_capabilities = sandbox
            .command(
                "/bin/sh",
                [
                    "-c",
                    &format!(
                        "printf '[{{\"id\":\"demo\"}}]' > '{0}.tmp' && mv '{0}.tmp' '{0}' && cat '{0}'",
                        capabilities.display()
                    ),
                ],
            )
            .output()
            .unwrap();
        assert!(
            edit_capabilities.status.success(),
            "editing capabilities.json should succeed: {}",
            String::from_utf8_lossy(&edit_capabilities.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&edit_capabilities.stdout).trim(),
            "[{\"id\":\"demo\"}]"
        );

        let read_db = sandbox
            .command("/bin/sh", ["-c", &format!("cat '{}'", db.display())])
            .output()
            .unwrap();
        assert!(
            !read_db.status.success(),
            "reading core/subject.db must fail"
        );

        let read_core_cfg = sandbox
            .command(
                "/bin/sh",
                ["-c", &format!("cat '{}'", core_config.display())],
            )
            .output()
            .unwrap();
        assert!(
            !read_core_cfg.status.success(),
            "reading core/config.json must fail"
        );

        let write_db = sandbox
            .command(
                "/bin/sh",
                ["-c", &format!("echo tampered >> '{}'", db.display())],
            )
            .output()
            .unwrap();
        assert!(
            !write_db.status.success(),
            "writing core/subject.db must fail"
        );
        assert_eq!(fs::read_to_string(&db).unwrap(), "secret-journal");

        let rename_core = sandbox
            .command(
                "/bin/sh",
                [
                    "-c",
                    &format!("mv '{}' '{}.moved'", core_dir.display(), core_dir.display()),
                ],
            )
            .output()
            .unwrap();
        assert!(!rename_core.status.success(), "renaming core dir must fail");

        let rename_parent = sandbox
            .command(
                "/bin/sh",
                ["-c", &format!("mv '{0}' '{0}.moved'", dir.path().display())],
            )
            .output()
            .unwrap();
        assert!(
            !rename_parent.status.success(),
            "renaming the protected directory's ancestor must fail"
        );

        let symlink_read = sandbox
            .command(
                "/bin/sh",
                [
                    "-c",
                    &format!(
                        "ln -s '{}' '{}/sym' && cat '{}/sym'",
                        db.display(),
                        data_dir.display(),
                        data_dir.display()
                    ),
                ],
            )
            .output()
            .unwrap();
        assert!(
            !symlink_read.status.success(),
            "reading core/subject.db via symlink must fail"
        );

        let hardlink = sandbox
            .command(
                "/bin/sh",
                [
                    "-c",
                    &format!("ln '{}' '{}/hard'", db.display(), data_dir.display()),
                ],
            )
            .output()
            .unwrap();
        assert!(
            !hardlink.status.success(),
            "hardlinking core/subject.db must fail"
        );

        let overwrite_exe = sandbox
            .command(
                "/bin/sh",
                ["-c", &format!("echo evil > '{}'", fake_exe.display())],
            )
            .output()
            .unwrap();
        assert!(
            !overwrite_exe.status.success(),
            "overwriting host binary must fail"
        );
        assert_eq!(fs::read_to_string(&fake_exe).unwrap(), "host-binary");

        let rename_exe = sandbox
            .command(
                "/bin/sh",
                [
                    "-c",
                    &format!("mv '{}' '{}.bak'", fake_exe.display(), fake_exe.display()),
                ],
            )
            .output()
            .unwrap();
        assert!(
            !rename_exe.status.success(),
            "renaming host binary must fail"
        );

        let parent_pid = std::process::id();
        let signal_parent = sandbox
            .command("/bin/sh", ["-c", &format!("kill -0 {parent_pid}")])
            .output()
            .unwrap();
        assert!(
            !signal_parent.status.success(),
            "signaling parent process must fail"
        );

        let signal_own_child = sandbox
            .command(
                "/bin/sh",
                ["-c", "sleep 10 & p=$!; kill -TERM $p; wait $p; exit 0"],
            )
            .output()
            .unwrap();
        assert!(
            signal_own_child.status.success(),
            "signaling process inside same sandbox should succeed: {}",
            String::from_utf8_lossy(&signal_own_child.stderr)
        );
    }

    #[test]
    fn seatbelt_protects_entire_app_bundle_for_writes_while_allowing_reads() {
        let dir = tempfile::tempdir().unwrap();
        let core_dir = dir.path().join("core");
        let bundle = dir.path().join("eja.app");
        let macos_dir = bundle.join("Contents/MacOS");
        let resources_dir = bundle.join("Contents/Resources");
        fs::create_dir_all(&core_dir).unwrap();
        fs::create_dir_all(&macos_dir).unwrap();
        fs::create_dir_all(&resources_dir).unwrap();

        let host_exe = macos_dir.join("eja-host");
        let app_exe = macos_dir.join("ejaApp");
        let protocol_doc = resources_dir.join("PROTOCOL.md");
        fs::write(&host_exe, "host").unwrap();
        fs::write(&app_exe, "app").unwrap();
        fs::write(&protocol_doc, "# Protocol v1\n").unwrap();

        let sandbox = Sandbox::with_host_exe(&core_dir, &host_exe).unwrap();

        let read_doc = sandbox
            .command(
                "/bin/sh",
                ["-c", &format!("cat '{}'", protocol_doc.display())],
            )
            .output()
            .unwrap();
        assert!(read_doc.status.success());
        assert!(String::from_utf8_lossy(&read_doc.stdout).contains("Protocol v1"));

        let write_app = sandbox
            .command(
                "/bin/sh",
                ["-c", &format!("echo evil > '{}'", app_exe.display())],
            )
            .output()
            .unwrap();
        assert!(
            !write_app.status.success(),
            "writing eja.app binary must fail"
        );

        let write_doc = sandbox
            .command(
                "/bin/sh",
                ["-c", &format!("echo evil > '{}'", protocol_doc.display())],
            )
            .output()
            .unwrap();
        assert!(
            !write_doc.status.success(),
            "writing eja.app Resources must fail"
        );
    }

    #[test]
    fn nofollow_file_helpers_refuse_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("eja");
        let core_dir = dir.path().join("ai.eja.kernel");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&core_dir).unwrap();
        let secret = core_dir.join("config.json");
        fs::write(&secret, r#"{"secret":true}"#).unwrap();
        let root = TrustedDir::open(&data_dir).unwrap();

        symlink(&secret, data_dir.join("capabilities.json")).unwrap();
        assert!(root.read_file("capabilities.json").is_err());

        root.write_file_atomic("capabilities.json", b"[]").unwrap();
        assert_eq!(fs::read_to_string(&secret).unwrap(), r#"{"secret":true}"#);
        assert_eq!(root.read_file("capabilities.json").unwrap(), b"[]");

        symlink(&core_dir, data_dir.join("outputs")).unwrap();
        assert!(root
            .write_new_file_in_subdir("outputs", "shell-1.log", b"evil".as_slice())
            .is_err());
        assert!(!core_dir.join("shell-1.log").exists());

        fs::remove_file(data_dir.join("outputs")).unwrap();
        fs::create_dir(data_dir.join("outputs")).unwrap();
        symlink(&secret, data_dir.join("outputs/shell-2.log")).unwrap();
        assert!(root
            .write_new_file_in_subdir("outputs", "shell-2.log", b"evil".as_slice())
            .is_err());
        assert_eq!(fs::read_to_string(&secret).unwrap(), r#"{"secret":true}"#);

        let ok_path = root
            .write_new_file_in_subdir("outputs", "shell-3.log", b"clean".as_slice())
            .unwrap();
        assert_eq!(fs::read_to_string(ok_path).unwrap(), "clean");

        let moved = dir.path().join("eja.moved");
        fs::rename(&data_dir, &moved).unwrap();
        symlink(&core_dir, &data_dir).unwrap();
        assert!(TrustedDir::open(&data_dir).is_err());
        assert!(root.read_file("config.json").is_err());
        root.write_file_atomic("capabilities.json", b"[1]").unwrap();
        assert!(!core_dir.join("capabilities.json").exists());
        assert_eq!(fs::read(moved.join("capabilities.json")).unwrap(), b"[1]");
        root.write_new_file_in_subdir("outputs", "shell-4.log", b"pinned".as_slice())
            .unwrap();
        assert!(!core_dir.join("outputs").exists());
        assert_eq!(
            fs::read_to_string(moved.join("outputs/shell-4.log")).unwrap(),
            "pinned"
        );
    }
}
