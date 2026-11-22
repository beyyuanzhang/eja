use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "macos")]
use std::collections::BTreeSet;
#[cfg(target_os = "macos")]
use std::ffi::OsString;
#[cfg(target_os = "macos")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "macos")]
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
#[cfg(target_os = "macos")]
const O_RDONLY: i32 = 0x0000;
#[cfg(target_os = "macos")]
const O_WRONLY: i32 = 0x0001;
#[cfg(target_os = "macos")]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(target_os = "macos")]
const O_CREAT: i32 = 0x0200;
#[cfg(target_os = "macos")]
const O_EXCL: i32 = 0x0800;
#[cfg(target_os = "macos")]
const O_DIRECTORY: i32 = 0x0010_0000;
#[cfg(target_os = "macos")]
const O_CLOEXEC: i32 = 0x0100_0000;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "macos")]
extern "C" {
    fn openat(dirfd: i32, path: *const std::ffi::c_char, oflag: i32, ...) -> i32;
    fn mkdirat(dirfd: i32, path: *const std::ffi::c_char, mode: u16) -> i32;
    fn renameat(
        fromfd: i32,
        from: *const std::ffi::c_char,
        tofd: i32,
        to: *const std::ffi::c_char,
    ) -> i32;
    fn unlinkat(dirfd: i32, path: *const std::ffi::c_char, flag: i32) -> i32;
}

fn validate_single_name(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid file name",
        ));
    }
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"))
}

#[cfg(target_os = "macos")]
fn open_dir(dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_CLOEXEC)
        .open(dir)
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

pub fn read_file_nofollow(dir: &Path, name: &str) -> io::Result<Vec<u8>> {
    let c_name = validate_single_name(name)?;
    #[cfg(target_os = "macos")]
    {
        let dir_file = open_dir(dir)?;
        let mut file = openat_file(
            dir_file.as_raw_fd(),
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
        let path = dir.join(name);
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
    dir: &Path,
    subdir: &str,
    name: &str,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let c_subdir = validate_single_name(subdir)?;
    let c_name = validate_single_name(name)?;
    #[cfg(target_os = "macos")]
    {
        let dir_file = open_dir(dir)?;
        if unsafe { mkdirat(dir_file.as_raw_fd(), c_subdir.as_ptr(), 0o755) } < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::AlreadyExists {
                return Err(err);
            }
        }
        let sub_file = openat_file(
            dir_file.as_raw_fd(),
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
        file.write_all(bytes)?;
        Ok(dir.join(subdir).join(name))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (c_subdir, c_name);
        let subdir_path = dir.join(subdir);
        match fs::create_dir(&subdir_path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
        if !fs::symlink_metadata(&subdir_path)?.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "subdir is not a real directory",
            ));
        }
        let target = subdir_path.join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        file.write_all(bytes)?;
        Ok(target)
    }
}

pub fn write_file_atomic_in_dir(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let c_name = validate_single_name(name)?;
    let pid = std::process::id();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(".{name}.{pid}.{seq}.tmp");
    let c_tmp = validate_single_name(&tmp_name)?;
    #[cfg(target_os = "macos")]
    {
        let dir_file = open_dir(dir)?;
        let dirfd = dir_file.as_raw_fd();
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
        let tmp_path = dir.join(&tmp_name);
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
        if let Err(err) = fs::rename(&tmp_path, dir.join(name)) {
            let _ = fs::remove_file(&tmp_path);
            return Err(err);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    #[cfg(target_os = "macos")]
    profile: String,
    #[cfg(target_os = "macos")]
    params: Vec<OsString>,
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
        Ok(Self { profile, params })
    }

    #[cfg(not(target_os = "macos"))]
    pub fn new(_protected_dir: &Path) -> Result<Self, String> {
        Ok(Self {})
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
            extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            unsafe {
                let _ = kill(-pid, 9);
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
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
        let data_dir = dir.path().join("Eja");
        let core_dir = dir.path().join("ai.eja.kernel");
        fs::create_dir(&data_dir).unwrap();
        fs::create_dir_all(&core_dir).unwrap();
        let db = core_dir.join("subject.db");
        fs::write(&db, "secret-journal").unwrap();
        let core_config = core_dir.join("config.json");
        fs::write(&core_config, "{\"api_key\":\"secret-key\"}").unwrap();
        let connectors = data_dir.join("connectors.json");
        fs::write(&connectors, "[]").unwrap();
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

        let edit_connectors = sandbox
            .command(
                "/bin/sh",
                [
                    "-c",
                    &format!(
                        "printf '[{{\"id\":\"demo\"}}]' > '{0}.tmp' && mv '{0}.tmp' '{0}' && cat '{0}'",
                        connectors.display()
                    ),
                ],
            )
            .output()
            .unwrap();
        assert!(
            edit_connectors.status.success(),
            "editing connectors.json should succeed: {}",
            String::from_utf8_lossy(&edit_connectors.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&edit_connectors.stdout).trim(),
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
        let bundle = dir.path().join("Eja.app");
        let macos_dir = bundle.join("Contents/MacOS");
        let resources_dir = bundle.join("Contents/Resources");
        fs::create_dir_all(&core_dir).unwrap();
        fs::create_dir_all(&macos_dir).unwrap();
        fs::create_dir_all(&resources_dir).unwrap();

        let host_exe = macos_dir.join("eja-host");
        let app_exe = macos_dir.join("EjaApp");
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
            "writing Eja.app binary must fail"
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
            "writing Eja.app Resources must fail"
        );
    }

    #[test]
    fn nofollow_file_helpers_refuse_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let core_dir = dir.path().join("ai.eja.kernel");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&core_dir).unwrap();
        let secret = core_dir.join("config.json");
        fs::write(&secret, r#"{"secret":true}"#).unwrap();

        symlink(&secret, data_dir.join("connectors.json")).unwrap();
        assert!(read_file_nofollow(&data_dir, "connectors.json").is_err());

        write_file_atomic_in_dir(&data_dir, "connectors.json", b"[]").unwrap();
        assert_eq!(fs::read_to_string(&secret).unwrap(), r#"{"secret":true}"#);
        assert_eq!(
            read_file_nofollow(&data_dir, "connectors.json").unwrap(),
            b"[]"
        );

        symlink(&core_dir, data_dir.join("outputs")).unwrap();
        assert!(write_new_file_in_subdir(&data_dir, "outputs", "shell-1.log", b"evil").is_err());
        assert!(!core_dir.join("shell-1.log").exists());

        fs::remove_file(data_dir.join("outputs")).unwrap();
        fs::create_dir(data_dir.join("outputs")).unwrap();
        symlink(&secret, data_dir.join("outputs/shell-2.log")).unwrap();
        assert!(write_new_file_in_subdir(&data_dir, "outputs", "shell-2.log", b"evil").is_err());
        assert_eq!(fs::read_to_string(&secret).unwrap(), r#"{"secret":true}"#);

        let ok_path =
            write_new_file_in_subdir(&data_dir, "outputs", "shell-3.log", b"clean").unwrap();
        assert_eq!(fs::read_to_string(ok_path).unwrap(), "clean");
    }
}
