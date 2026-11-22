use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

#[cfg(target_os = "macos")]
use std::collections::BTreeSet;
#[cfg(target_os = "macos")]
use std::ffi::OsString;
#[cfg(target_os = "macos")]
use std::fs;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

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
        let ancestors = collect_ancestors(&[protected.as_path(), host_exe.as_path()]);
        let (profile, params) = build_seatbelt_policy(&protected, &host_exe, &ancestors);
        Ok(Self { profile, params })
    }

    #[cfg(not(target_os = "macos"))]
    pub fn new(_protected_dir: &Path) -> Result<Self, String> {
        Ok(Self {})
    }

    pub fn command<I, S>(&self, program: &str, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        #[cfg(target_os = "macos")]
        {
            let mut cmd = Command::new(SANDBOX_EXEC);
            cmd.arg("-p").arg(&self.profile);
            cmd.args(&self.params);
            cmd.arg("--").arg(program).args(args);
            cmd
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd
        }
    }
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
    ancestors: &BTreeSet<PathBuf>,
) -> (String, Vec<OsString>) {
    let mut profile = String::from(
        "(version 1)\n\
         (allow default)\n\
         (deny signal)\n\
         (allow signal (target same-sandbox))\n\
         (deny process-info-pidinfo)\n\
         (allow process-info-pidinfo (target same-sandbox))\n\
         (deny file-read* file-write* (subpath (param \"PROTECTED_DIR\")))\n\
         (deny file-write* (literal (param \"HOST_EXE\")))\n",
    );
    let mut params = vec![
        define_param("PROTECTED_DIR", protected_dir),
        define_param("HOST_EXE", host_exe),
    ];
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
        let data_dir = dir.path();
        let core_dir = data_dir.join("core");
        fs::create_dir_all(&core_dir).unwrap();
        let db = core_dir.join("subject.db");
        fs::write(&db, "secret-journal").unwrap();
        let core_config = core_dir.join("config.json");
        fs::write(&core_config, "{\"api_key\":\"secret-key\"}").unwrap();
        let connectors = data_dir.join("connectors.json");
        fs::write(&connectors, "[]").unwrap();
        let fake_exe = data_dir.join("eja-host");
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
                [
                    "-c",
                    &format!("mv '{}' '{}.moved'", data_dir.display(), data_dir.display()),
                ],
            )
            .output()
            .unwrap();
        assert!(
            !rename_parent.status.success(),
            "renaming ancestor data_dir must fail"
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
}
