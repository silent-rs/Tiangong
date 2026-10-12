use std::path::PathBuf;

use anyhow::{Context, Result};

use super::model::{Job, JobRun, JobRunStatus, UpdateJobRequest};

/// Job 存储（JSON 文件）
///
/// 仅持有两个路径，派生 `Clone` 便于把同一存储根传给后台执行任务（如 Agent 触发
/// 后 `tokio::spawn(execute_job_with_store(..., store.clone()))`）。
#[derive(Clone)]
pub struct JobStore {
    jobs_path: PathBuf,
    runs_dir: PathBuf,
}

impl JobStore {
    /// 在指定目录打开或创建存储
    pub fn open_at(base: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&base)
            .with_context(|| format!("创建调度器目录失败: {}", base.display()))?;
        let runs_dir = base.join("runs");
        std::fs::create_dir_all(&runs_dir)
            .with_context(|| format!("创建运行记录目录失败: {}", runs_dir.display()))?;
        let jobs_path = base.join("jobs.json");
        let store = Self {
            jobs_path,
            runs_dir,
        };
        store.ensure_jobs_file()?;
        Ok(store)
    }

    // ── Job CRUD ──────────────────────────────────────────────────

    pub fn insert_job(&self, job: &Job) -> Result<Job> {
        let mut job = job.clone();
        job.name = crate::normalize_required_single_line("任务名称", &job.name)?;
        job.description = crate::normalize_required_single_line("任务描述", &job.description)?;
        let mut jobs = self.load_jobs()?;
        jobs.push(job.clone());
        self.save_jobs(&jobs)?;
        Ok(job)
    }

    pub fn get_job(&self, id: &str) -> Result<Option<Job>> {
        let jobs = self.load_jobs()?;
        Ok(jobs.into_iter().find(|j| j.id == id))
    }

    pub fn list_jobs(&self) -> Result<Vec<Job>> {
        let mut jobs = self.load_jobs()?;
        jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(jobs)
    }

    pub fn list_enabled_cron_jobs(&self) -> Result<Vec<Job>> {
        let mut jobs = self.load_jobs()?;
        jobs.retain(|j| j.enabled);
        jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(jobs)
    }

    pub fn update_job(&self, id: &str, req: &UpdateJobRequest) -> Result<bool> {
        let mut jobs = self.load_jobs()?;
        let now = chrono::Local::now().naive_local().to_string();
        let Some(job) = jobs.iter_mut().find(|j| j.id == id) else {
            return Ok(false);
        };
        let name = req
            .name
            .as_deref()
            .map(|value| crate::normalize_required_single_line("任务名称", value))
            .transpose()?;
        let description = req
            .description
            .as_deref()
            .map(|value| crate::normalize_required_single_line("任务描述", value))
            .transpose()?;
        job.updated_at = now;
        if let Some(v) = name {
            job.name = v;
        }
        if let Some(v) = description {
            job.description = v;
        }
        if let Some(ref v) = req.schedule {
            job.schedule = Some(v.clone());
        }
        if let Some(ref v) = req.session_id {
            job.session_id = Some(v.clone());
        }
        if let Some(ref v) = req.payload {
            job.payload = v.clone();
        }
        if let Some(v) = req.enabled {
            job.enabled = v;
        }
        self.save_jobs(&jobs)?;
        Ok(true)
    }

    pub fn delete_job(&self, id: &str) -> Result<bool> {
        let mut jobs = self.load_jobs()?;
        let before = jobs.len();
        jobs.retain(|j| j.id != id);
        if jobs.len() == before {
            return Ok(false);
        }
        self.save_jobs(&jobs)?;
        let run_file = self.runs_dir.join(format!("{id}.json"));
        let _ = std::fs::remove_file(run_file);
        Ok(true)
    }

    // ── JobRun CRUD ───────────────────────────────────────────────

    pub fn insert_job_run(&self, run: &JobRun) -> Result<()> {
        let mut runs = self.load_runs(&run.job_id)?;
        runs.push(run.clone());
        self.save_runs(&run.job_id, &runs)
    }

    pub fn update_job_run_status(
        &self,
        id: &str,
        job_id: &str,
        status: &JobRunStatus,
        finished_at: Option<&str>,
        result_summary: Option<&str>,
    ) -> Result<bool> {
        let mut runs = self.load_runs(job_id)?;
        let Some(run) = runs.iter_mut().find(|r| r.id == id) else {
            return Ok(false);
        };
        run.status = status.clone();
        if let Some(t) = finished_at {
            run.finished_at = Some(t.to_string());
        }
        if let Some(s) = result_summary {
            run.result_summary = Some(s.to_string());
        }
        self.save_runs(job_id, &runs)?;
        Ok(true)
    }

    pub fn list_job_runs(&self, job_id: &str, limit: usize) -> Result<Vec<JobRun>> {
        let mut runs = self.load_runs(job_id)?;
        runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        runs.truncate(limit);
        Ok(runs)
    }

    // ── 内部方法 ──────────────────────────────────────────────────

    fn ensure_jobs_file(&self) -> Result<()> {
        if !self.jobs_path.exists() {
            atomic_write(&self.jobs_path, "[]").with_context(|| "初始化 jobs.json 失败")?;
        }
        Ok(())
    }

    fn load_jobs(&self) -> Result<Vec<Job>> {
        let content = std::fs::read_to_string(&self.jobs_path)
            .with_context(|| format!("读取 {} 失败", self.jobs_path.display()))?;
        let jobs: Vec<Job> = serde_json::from_str(&content)
            .with_context(|| format!("解析 {} 失败", self.jobs_path.display()))?;
        Ok(jobs)
    }

    fn save_jobs(&self, jobs: &[Job]) -> Result<()> {
        let content = serde_json::to_string_pretty(jobs).with_context(|| "序列化 jobs 失败")?;
        atomic_write(&self.jobs_path, &content)
            .with_context(|| format!("写入 {} 失败", self.jobs_path.display()))?;
        Ok(())
    }

    fn load_runs(&self, job_id: &str) -> Result<Vec<JobRun>> {
        let path = self.runs_dir.join(format!("{job_id}.json"));
        if !path.exists() {
            return Ok(vec![]);
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        let runs: Vec<JobRun> = serde_json::from_str(&content)
            .with_context(|| format!("解析 {} 失败", path.display()))?;
        Ok(runs)
    }

    fn save_runs(&self, job_id: &str, runs: &[JobRun]) -> Result<()> {
        let path = self.runs_dir.join(format!("{job_id}.json"));
        let content = serde_json::to_string_pretty(runs).with_context(|| "序列化 runs 失败")?;
        atomic_write(&path, &content).with_context(|| format!("写入 {} 失败", path.display()))?;
        Ok(())
    }
}

/// 原子写入：先写临时文件再 rename，防止写入中断导致文件损坏
fn atomic_write(path: &std::path::Path, content: &str) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, content)
        .with_context(|| format!("写入临时文件 {} 失败", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("重命名 {} → {} 失败", tmp_path.display(), path.display()))?;
    Ok(())
}
