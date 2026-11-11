use eja::adapters::PiWorker;
use eja::{Assignment, ReportStatus, Worker};
use std::path::PathBuf;

fn assignment() -> Assignment {
    Assignment {
        task_id: 1,
        objective: "RAW USER TASK".to_string(),
        issued_at: 1_700_000_000,
    }
}

fn script(dir: &std::path::Path, body: &str) -> PathBuf {
    let path = dir.join("fake-pi.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path).expect("meta").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod");
    }
    path
}

#[test]
fn pi_worker_passes_the_raw_task_and_system_prompt() {
    let dir = std::env::temp_dir().join(format!("eja-pi-worker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let args_file = dir.join("args.txt");
    let script = script(
        &dir,
        &format!(
            "printf '%s\\n' \"$@\" > '{}'; printf 'artifact' > out.txt",
            args_file.display()
        ),
    );
    let worker = PiWorker::with_command(script.display().to_string())
        .with_system_prompt("WORKER SYSTEM PROMPT")
        .with_cwd(&dir)
        .with_timeout(std::time::Duration::from_secs(5));
    let report = worker.execute(&assignment());
    assert_eq!(report.status, ReportStatus::Done);
    let args = std::fs::read_to_string(&args_file).expect("args");
    assert!(args.contains("RAW USER TASK"));
    assert!(args.contains("WORKER SYSTEM PROMPT"));
    assert!(!args.contains("title only"));
    assert!(!args.contains("acceptance"));
    assert!(report
        .artifacts
        .iter()
        .any(|artifact| artifact == "out.txt"));
}

#[test]
fn pi_worker_timeout_kills_the_session() {
    let dir = std::env::temp_dir().join(format!("eja-pi-timeout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let script = script(&dir, "sleep 10");
    let worker = PiWorker::with_command(script.display().to_string())
        .with_timeout(std::time::Duration::from_millis(100));
    let report = worker.execute(&assignment());
    assert_eq!(report.status, ReportStatus::Failed);
    assert!(report.summary.contains("timed out"));
}
