use std::path::PathBuf;
use tempfile::TempDir;
use tiangong_scheduler::model::{Job, JobRun, JobRunStatus, TriggerType, UpdateJobRequest};
use tiangong_scheduler::store::JobStore;

fn setup() -> (TempDir, JobStore) {
    let dir = TempDir::new().unwrap();
    let store = JobStore::open_at(dir.path().to_path_buf()).unwrap();
    (dir, store)
}

fn sample_job(id: &str) -> Job {
    Job {
        id: id.to_string(),
        name: "测试任务".to_string(),
        description: "测试描述".to_string(),
        trigger_type: TriggerType::Cron,
        schedule: Some("0 */1 * * * *".to_string()),
        session_id: None,
        payload: "hello".to_string(),
        enabled: true,
        created_at: chrono::Local::now().naive_local().to_string(),
        updated_at: chrono::Local::now().naive_local().to_string(),
    }
}

#[test]
fn insert_and_get_job() {
    let (_dir, store) = setup();
    let mut job = sample_job("job-1");
    job.name = "  第一行\r\n第二行\n\n第三行  ".to_string();
    job.description = " 描述一\r描述二 ".to_string();
    job.payload = "执行第一行\r\n执行第二行".to_string();
    let saved = store.insert_job(&job).unwrap();

    assert_eq!(saved.name, "第一行 第二行 第三行");
    assert_eq!(saved.description, "描述一 描述二");
    assert_eq!(saved.payload, "执行第一行\r\n执行第二行");

    let got = store.get_job("job-1").unwrap().unwrap();
    assert_eq!(got.id, "job-1");
    assert_eq!(got.name, "第一行 第二行 第三行");
    assert_eq!(got.description, "描述一 描述二");
    assert_eq!(got.payload, "执行第一行\r\n执行第二行");
    assert!(got.session_id.is_none());

    let mut empty_job = sample_job("empty-name");
    empty_job.name = " \r\n ".to_string();
    let error = store.insert_job(&empty_job).unwrap_err().to_string();
    assert!(error.contains("任务名称"), "错误应指出空字段：{error}");
}

#[test]
fn update_job_normalizes_display_fields_without_changing_payload() {
    let (_dir, store) = setup();
    store
        .insert_job(&sample_job("job-normalize-update"))
        .unwrap();

    store
        .update_job(
            "job-normalize-update",
            &UpdateJobRequest {
                name: Some(" 新版\n任务 ".to_string()),
                description: Some(" 描述一\r\n\r描述二 ".to_string()),
                payload: Some("执行第一行\n执行第二行".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    let job = store.get_job("job-normalize-update").unwrap().unwrap();
    assert_eq!(job.name, "新版 任务");
    assert_eq!(job.description, "描述一 描述二");
    assert_eq!(job.payload, "执行第一行\n执行第二行");

    let error = store
        .update_job(
            "job-normalize-update",
            &UpdateJobRequest {
                description: Some(" \n\r ".to_string()),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("任务描述"), "错误应指出空字段：{error}");

    let job = store.get_job("job-normalize-update").unwrap().unwrap();
    assert_eq!(job.description, "描述一 描述二");
}

#[test]
fn update_session_id_persists() {
    let (_dir, store) = setup();
    let job = sample_job("job-2");
    store.insert_job(&job).unwrap();

    // 模拟 pin_session_to_tracker：写入 session_id
    store
        .update_job(
            "job-2",
            &UpdateJobRequest {
                session_id: Some("session-abc".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    // 重新从磁盘加载验证持久化
    let store2 = JobStore::open_at(PathBuf::from(_dir.path())).unwrap();
    let got = store2.get_job("job-2").unwrap().unwrap();
    assert_eq!(got.session_id.as_deref(), Some("session-abc"));
}

#[test]
fn session_reuse_after_pin() {
    let (_dir, store) = setup();
    let job = sample_job("job-3");
    store.insert_job(&job).unwrap();

    // 首次执行后 pin session
    store
        .update_job(
            "job-3",
            &UpdateJobRequest {
                session_id: Some("session-xyz".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    // 模拟 execute_job：重新从 store 加载 job
    let fresh = store.get_job("job-3").unwrap().unwrap();
    assert_eq!(fresh.session_id.as_deref(), Some("session-xyz"));
}

#[test]
fn list_enabled_cron_jobs() {
    let (_dir, store) = setup();
    store.insert_job(&sample_job("j1")).unwrap();

    let mut disabled = sample_job("j2");
    disabled.enabled = false;
    store.insert_job(&disabled).unwrap();

    let mut another = sample_job("j3");
    another.enabled = false;
    store.insert_job(&another).unwrap();

    let enabled = store.list_enabled_cron_jobs().unwrap();
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].id, "j1");
}

#[test]
fn job_run_lifecycle() {
    let (_dir, store) = setup();
    store.insert_job(&sample_job("job-r")).unwrap();

    // 开始运行
    let run = JobRun {
        id: "run-1".to_string(),
        job_id: "job-r".to_string(),
        session_id: "sess-1".to_string(),
        status: JobRunStatus::Running,
        started_at: "2026-01-01 00:00:00".to_string(),
        finished_at: None,
        result_summary: None,
    };
    store.insert_job_run(&run).unwrap();

    let runs = store.list_job_runs("job-r", 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert!(matches!(runs[0].status, JobRunStatus::Running));

    // 执行成功
    store
        .update_job_run_status(
            "run-1",
            "job-r",
            &JobRunStatus::Succeeded,
            Some("2026-01-01 00:00:05"),
            Some("Hello"),
        )
        .unwrap();

    let runs = store.list_job_runs("job-r", 10).unwrap();
    assert!(matches!(runs[0].status, JobRunStatus::Succeeded));
    assert_eq!(runs[0].result_summary.as_deref(), Some("Hello"));
}

#[test]
fn update_job_preserves_other_fields() {
    let (_dir, store) = setup();
    let job = sample_job("job-p");
    store.insert_job(&job).unwrap();

    // 只更新 session_id
    store
        .update_job(
            "job-p",
            &UpdateJobRequest {
                session_id: Some("sess-new".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    let got = store.get_job("job-p").unwrap().unwrap();
    assert_eq!(got.session_id.as_deref(), Some("sess-new"));
    assert_eq!(got.name, "测试任务"); // 其他字段不变
    assert_eq!(got.payload, "hello");
}
