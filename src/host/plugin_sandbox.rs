//! Launch an authored Python MCP server with no workspace or service authority.

use std::path::Path;

#[cfg(any(target_os = "macos", target_os = "linux"))]
const BOOTSTRAP: &str = r"
import resource, runpy, sys
resource.setrlimit(resource.RLIMIT_CPU, (15, 15))
resource.setrlimit(resource.RLIMIT_NOFILE, (64, 64))
if sys.platform == 'linux':
    resource.setrlimit(resource.RLIMIT_AS, (536870912, 536870912))
sys.argv = sys.argv[1:]
runpy.run_path(sys.argv[0], run_name='__main__')
";

#[cfg(target_os = "macos")]
pub(super) fn command(source: &Path) -> eja::Result<(&'static str, Vec<String>)> {
    const PROFILE: &str = r#"
(version 1)
(deny default)
(import "dyld-support.sb")
(allow process-exec (literal (param "PYTHON")))
(allow file-read* file-map-executable
    (subpath "/System/Library")
    (subpath "/usr/lib")
    (subpath "/usr/share")
    (subpath "/Library/Developer/CommandLineTools/Library/Frameworks")
    (subpath "/Applications/Xcode.app/Contents/Developer/Library/Frameworks"))
(allow file-read* (literal (param "SOURCE"))
    (literal "/dev/null") (literal "/dev/random") (literal "/dev/urandom"))
(allow file-read-metadata)
(allow sysctl-read)
"#;
    let python = [
        "/Library/Developer/CommandLineTools/usr/bin/python3",
        "/Applications/Xcode.app/Contents/Developer/usr/bin/python3",
    ]
    .iter()
    .find_map(|path| {
        let shim = Path::new(path).canonicalize().ok()?;
        // Apple's framework launcher normally spawns this executable. Run it
        // directly so the sandbox need not allow arbitrary child processes.
        let python = shim
            .parent()?
            .parent()?
            .join("Resources/Python.app/Contents/MacOS/Python");
        python.canonicalize().ok()
    })
    .ok_or("managed plugins need Python 3 from the macOS Command Line Tools or Xcode")?;
    let python = python.to_str().ok_or("invalid Python path")?;
    let source = source.to_str().ok_or("invalid plugin source path")?;
    Ok((
        "/usr/bin/sandbox-exec",
        vec![
            "-D".into(),
            format!("PYTHON={python}"),
            "-D".into(),
            format!("SOURCE={source}"),
            "-p".into(),
            PROFILE.into(),
            python.into(),
            "-I".into(),
            "-B".into(),
            "-u".into(),
            "-c".into(),
            BOOTSTRAP.into(),
            source.into(),
        ],
    ))
}

#[cfg(target_os = "linux")]
pub(super) fn command(source: &Path) -> eja::Result<(&'static str, Vec<String>)> {
    const LAUNCHER: &str = "/usr/bin/bwrap";
    if !Path::new(LAUNCHER).is_file() {
        return Err("managed plugins need Bubblewrap at /usr/bin/bwrap".into());
    }
    let source = source.to_str().ok_or("invalid plugin source path")?;
    let args = [
        "--unshare-all",
        "--disable-userns",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--ro-bind",
        "/usr/bin/python3",
        "/usr/bin/python3",
        "--ro-bind",
        "/usr/lib",
        "/usr/lib",
        "--ro-bind-try",
        "/lib",
        "/lib",
        "--ro-bind-try",
        "/lib64",
        "/lib64",
        "--dev",
        "/dev",
        "--ro-bind",
        source,
        "/plugin.py",
        "--chdir",
        "/",
        "/usr/bin/python3",
        "-I",
        "-B",
        "-u",
        "-c",
        BOOTSTRAP,
        "/plugin.py",
    ];
    Ok((LAUNCHER, args.into_iter().map(str::to_owned).collect()))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn command(_: &Path) -> eja::Result<(&'static str, Vec<String>)> {
    Err("no managed plugin sandbox is available on this platform".into())
}
