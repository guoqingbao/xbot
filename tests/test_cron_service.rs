use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use tempfile::tempdir;
use xbot::cron::{CronSchedule, CronScheduleKind, CronService};

#[test]
fn add_job_rejects_unknown_timezone() {
    let dir = tempdir().unwrap();
    let service = CronService::new(dir.path().join("jobs.json"));
    let result = service.add_job(
        "tz typo",
        CronSchedule {
            kind: CronScheduleKind::Cron,
            expr: Some("0 9 * * *".to_string()),
            tz: Some("America/Vancovuer".to_string()),
            ..CronSchedule::default()
        },
        "hello",
        false,
        None,
        None,
        false,
    );
    assert!(result.is_err());
    assert!(service.list_jobs(true).unwrap().is_empty());
}

#[test]
fn add_job_accepts_valid_timezone() {
    let dir = tempdir().unwrap();
    let service = CronService::new(dir.path().join("jobs.json"));
    let job = service
        .add_job(
            "tz ok",
            CronSchedule {
                kind: CronScheduleKind::Cron,
                expr: Some("0 9 * * *".to_string()),
                tz: Some("America/Vancouver".to_string()),
                ..CronSchedule::default()
            },
            "hello",
            false,
            None,
            None,
            false,
        )
        .unwrap();
    assert_eq!(job.schedule.tz.as_deref(), Some("America/Vancouver"));
    assert!(job.state.next_run_at_ms.is_some());
}

#[tokio::test]
async fn execute_job_records_run_history() {
    let dir = tempdir().unwrap();
    let service =
        CronService::with_callback(dir.path().join("jobs.json"), |_job| async move { Ok(()) });
    let job = service
        .add_job(
            "hist",
            CronSchedule {
                kind: CronScheduleKind::Every,
                every_ms: Some(60_000),
                ..CronSchedule::default()
            },
            "hello",
            false,
            None,
            None,
            false,
        )
        .unwrap();
    service.run_job(&job.id, false).await.unwrap();
    let loaded = service.get_job(&job.id).unwrap().unwrap();
    assert_eq!(loaded.state.run_history.len(), 1);
    let rec = &loaded.state.run_history[0];
    assert_eq!(rec.status, "ok");
    assert!(rec.duration_ms < 60_000);
    assert!(rec.error.is_none());
}

#[tokio::test]
async fn run_history_records_errors() {
    let dir = tempdir().unwrap();
    let service = CronService::with_callback(dir.path().join("jobs.json"), |_job| async move {
        Err(anyhow!("boom"))
    });
    let job = service
        .add_job(
            "fail",
            CronSchedule {
                kind: CronScheduleKind::Every,
                every_ms: Some(60_000),
                ..CronSchedule::default()
            },
            "hello",
            false,
            None,
            None,
            false,
        )
        .unwrap();
    service.run_job(&job.id, false).await.unwrap();
    let loaded = service.get_job(&job.id).unwrap().unwrap();
    assert_eq!(loaded.state.run_history.len(), 1);
    assert_eq!(loaded.state.run_history[0].status, "error");
    assert_eq!(loaded.state.run_history[0].error.as_deref(), Some("boom"));
}

#[tokio::test]
async fn run_history_trimmed_to_max() {
    let dir = tempdir().unwrap();
    let service =
        CronService::with_callback(dir.path().join("jobs.json"), |_job| async move { Ok(()) });
    let job = service
        .add_job(
            "trim",
            CronSchedule {
                kind: CronScheduleKind::Every,
                every_ms: Some(60_000),
                ..CronSchedule::default()
            },
            "hello",
            false,
            None,
            None,
            false,
        )
        .unwrap();
    for _ in 0..25 {
        service.run_job(&job.id, false).await.unwrap();
    }
    let loaded = service.get_job(&job.id).unwrap().unwrap();
    assert_eq!(loaded.state.run_history.len(), CronService::MAX_RUN_HISTORY);
}

#[tokio::test]
async fn run_history_persisted_to_disk() {
    let dir = tempdir().unwrap();
    let store_path = dir.path().join("jobs.json");
    let service = CronService::with_callback(&store_path, |_job| async move { Ok(()) });
    let job = service
        .add_job(
            "persist",
            CronSchedule {
                kind: CronScheduleKind::Every,
                every_ms: Some(60_000),
                ..CronSchedule::default()
            },
            "hello",
            false,
            None,
            None,
            false,
        )
        .unwrap();
    service.run_job(&job.id, false).await.unwrap();
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    let history = raw["jobs"][0]["state"]["runHistory"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    let fresh = CronService::new(&store_path);
    let loaded = fresh.get_job(&job.id).unwrap().unwrap();
    assert_eq!(loaded.state.run_history.len(), 1);
}

#[tokio::test]
async fn running_service_honors_external_disable() -> Result<()> {
    let dir = tempdir().unwrap();
    let store_path = dir.path().join("jobs.json");
    let called = Arc::new(Mutex::new(Vec::<String>::new()));
    let called_clone = called.clone();
    let service = CronService::with_callback(&store_path, move |job| {
        let called = called_clone.clone();
        async move {
            called.lock().unwrap().push(job.id);
            Ok(())
        }
    });
    let job = service.add_job(
        "external-disable",
        CronSchedule {
            kind: CronScheduleKind::Every,
            every_ms: Some(200),
            ..CronSchedule::default()
        },
        "hello",
        false,
        None,
        None,
        false,
    )?;
    service.start().await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let external = CronService::new(&store_path);
    let updated = external.enable_job(&job.id, false)?.unwrap();
    assert!(!updated.enabled);
    tokio::time::sleep(Duration::from_millis(350)).await;
    service.stop();
    assert!(called.lock().unwrap().is_empty());
    Ok(())
}
