use eja::adapters::{MissingWorker, SessionWorker};
use eja::{Assignment, ReportStatus, Worker};

fn assignment() -> Assignment {
    Assignment {
        task_id: 1,
        objective: "do it".to_string(),
        issued_at: 1_700_000_000,
    }
}

#[test]
fn command_worker_protocol_returns_a_report() {
    let dir = std::env::temp_dir().join(format!("eja-worker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let script = dir.join("ajas.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"status\":\"done\",\"summary\":\"ok\",\"artifacts\":[\"a.txt\"],\"evidence\":[],\"open_questions\":[]}'\n",
    )
    .expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).expect("meta").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
    }
    let worker = SessionWorker::new(format!("sh {}", script.display()));
    let report = worker.execute(&assignment());
    assert_eq!(report.status, ReportStatus::Done);
    assert_eq!(report.artifacts, vec!["a.txt".to_string()]);

    let failed = SessionWorker::new("exit 7").execute(&assignment());
    assert_eq!(failed.status, ReportStatus::Failed);

    let invalid = SessionWorker::new("printf 'not json'").execute(&assignment());
    assert_eq!(invalid.status, ReportStatus::Failed);

    let missing = MissingWorker.execute(&assignment());
    assert_eq!(missing.status, ReportStatus::Failed);
    assert!(missing.summary.contains("EJA_AJA_COMMAND"));
}

#[test]
fn session_worker_times_out_instead_of_hanging_eja() {
    let worker = SessionWorker::with_timeout("sleep 2", std::time::Duration::from_millis(100));
    let started = std::time::Instant::now();
    let report = worker.execute(&assignment());
    assert_eq!(report.status, ReportStatus::Failed);
    assert!(report.summary.contains("timed out"));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[test]
fn worker_box_kills_background_processes_after_a_normal_exit() {
    let dir = std::env::temp_dir().join(format!("eja-worker-bg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let artifact = dir.join("late.txt");
    let script = dir.join("ajas.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat >/dev/null\n( sleep 1; echo late > '{}' ) &\nprintf '%s' '{{\"status\":\"done\",\"summary\":\"ok\",\"artifacts\":[],\"evidence\":[],\"open_questions\":[]}}'\n",
            artifact.display()
        ),
    )
    .expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).expect("meta").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
    }
    let worker = SessionWorker::with_timeout(
        format!("sh {}", script.display()),
        std::time::Duration::from_secs(5),
    );
    let report = worker.execute(&assignment());
    assert_eq!(report.status, ReportStatus::Done);
    std::thread::sleep(std::time::Duration::from_millis(1400));
    assert!(!artifact.exists(), "background writer survived worker exit");
}

#[test]
fn worker_box_kills_background_processes_on_timeout() {
    let dir = std::env::temp_dir().join(format!("eja-worker-bg-timeout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let artifact = dir.join("late.txt");
    let script = dir.join("ajas.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat >/dev/null\n( sleep 1; echo late > '{}' ) &\nsleep 10\n",
            artifact.display()
        ),
    )
    .expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).expect("meta").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
    }
    let worker = SessionWorker::with_timeout(
        format!("sh {}", script.display()),
        std::time::Duration::from_millis(100),
    );
    let report = worker.execute(&assignment());
    assert_eq!(report.status, ReportStatus::Failed);
    assert!(report.summary.contains("timed out"));
    std::thread::sleep(std::time::Duration::from_millis(1400));
    assert!(
        !artifact.exists(),
        "background writer survived worker timeout"
    );
}
