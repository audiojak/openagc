//! Routines across the FFI (spec §11). The definition crosses as JSON (the
//! `agent_api::routines::Routine` shape); Swift decodes it into its own
//! Codable mirror for the editor.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_api::routines::{Identity, PromptTarget, Routine, Runner, generate_prompt, schedule};
use mail_store::routines::{self as store, RoutineRow};

use crate::{Core, CoreError, ErrorKind, runtime};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RoutineInfo {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// `claude_cloud`, `claude_desktop`, `chat_gpt_cloud` or `local`.
    pub runner: String,
    pub definition_json: String,
    /// The generated prompt differs from what was last published.
    pub changed_since_publish: bool,
    pub cloud_url: Option<String>,
    pub updated_at: i64,
}

fn runner_name(r: Runner) -> String {
    serde_json::to_value(r).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

fn parse_runner(name: &str) -> Result<Runner, CoreError> {
    serde_json::from_value(serde_json::Value::String(name.to_owned()))
        .map_err(|_| CoreError::new(ErrorKind::InvalidInput, format!("unknown runner {name:?}")))
}

pub(crate) fn decode(json: &str) -> Result<Routine, CoreError> {
    serde_json::from_str(json).map_err(|e| CoreError::new(ErrorKind::InvalidInput, format!("bad routine: {e}")))
}

fn info(row: RoutineRow) -> RoutineInfo {
    let changed = decode(&row.definition_json)
        .map(|r| r.cloud.published_fingerprint.is_some() && r.cloud.published_fingerprint != row.sync_fingerprint)
        .unwrap_or(false);
    RoutineInfo {
        id: row.uuid,
        name: row.name,
        enabled: row.enabled,
        runner: row.runner,
        definition_json: row.definition_json,
        changed_since_publish: changed,
        cloud_url: row.cloud_url,
        updated_at: row.updated_at,
    }
}

impl Core {
    /// Store a routine, recomputing its prompt fingerprint.
    pub(crate) async fn store_routine(&self, routine: &Routine) -> Result<RoutineInfo, CoreError> {
        let problems = routine.validate();
        if !problems.is_empty() {
            return Err(CoreError::new(ErrorKind::InvalidInput, problems.join(" ")));
        }
        let target = agent_api::routines::PromptTarget::Runner(routine.runner);
        let fingerprint =
            agent_api::routines::prompt_fingerprint(&agent_api::routines::generate_prompt(routine, target));
        let now = mail_sync::now_millis();
        let row = RoutineRow {
            uuid: routine.id.clone(),
            name: routine.name.clone(),
            enabled: routine.enabled,
            runner: runner_name(routine.runner),
            template_id: routine.template.as_ref().map(|t| t.id.clone()),
            template_version: routine.template.as_ref().map(|t| t.version),
            definition_json: serde_json::to_string(routine)
                .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?,
            sync_fingerprint: Some(fingerprint),
            cloud_url: routine.cloud.routine_url.clone(),
            created_at: now,
            updated_at: now,
        };
        let db = self.db()?;
        let saved = row.clone();
        runtime::run(async move { Ok(db.write(move |tx| store::save(tx, &saved)).await?) }).await?;
        Ok(info(row))
    }

    pub(crate) async fn load_routine(&self, id: &str) -> Result<Routine, CoreError> {
        let db = self.db()?;
        let id = id.to_owned();
        let row = runtime::run(async move { Ok(db.read(move |c| store::get(c, &id)).await?) })
            .await?
            .ok_or_else(|| CoreError::new(ErrorKind::NotFound, "no such routine"))?;
        decode(&row.definition_json)
    }
}

#[uniffi::export]
impl Core {
    /// A new routine from the *Sort important mail* template, filled with
    /// this account's address, and stored.
    pub async fn create_routine_from_template(&self, runner: String) -> Result<RoutineInfo, CoreError> {
        let runner = parse_runner(&runner)?;
        let email = self.account_address().await?;
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        let id = format!("routine-{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>());
        let identity = Identity { primary_email: email, aliases: vec![], frequently_cc: vec![] };
        self.store_routine(&Routine::sort_important(id, identity, runner)).await
    }

    pub async fn list_routines(&self) -> Result<Vec<RoutineInfo>, CoreError> {
        let db = self.db()?;
        runtime::run(async move { Ok(db.read(store::list).await?.into_iter().map(info).collect()) }).await
    }

    /// Save an edited routine (its JSON, as `definition_json` came).
    pub async fn save_routine(&self, definition_json: String) -> Result<RoutineInfo, CoreError> {
        let routine = decode(&definition_json)?;
        self.store_routine(&routine).await
    }

    pub async fn delete_routine(&self, id: String) -> Result<(), CoreError> {
        let db = self.db()?;
        runtime::run(async move { Ok(db.write(move |tx| store::delete(tx, &id)).await?) }).await
    }

    /// The prompt the routine's runner will be given.
    pub async fn routine_prompt(&self, id: String) -> Result<String, CoreError> {
        let routine = self.load_routine(&id).await?;
        Ok(agent_api::routines::generate_prompt(&routine, agent_api::routines::PromptTarget::Runner(routine.runner)))
    }
}

// MARK: Local runs (spec §11.5, §11.7)

/// A running routine session.
#[derive(Debug, Clone)]
pub(crate) struct RoutineSession {
    pub routine_id: String,
    /// `None` for a preview.
    pub run_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RoutinePreviewRow {
    pub thread_id: String,
    /// `None`: leave it in the inbox.
    pub bucket_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RoutineRunInfo {
    pub run_id: i64,
    pub inferred: bool,
    pub session_id: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// `running`, `succeeded`, `failed`, `missed`, `undone`, …
    pub status: String,
    /// `{bucket_id: count}` as JSON.
    pub counts_json: String,
    pub report_text: Option<String>,
    pub thread_count: u32,
}

/// Parse a preview's reply: a JSON array, possibly wrapped in a code fence
/// or surrounded by a sentence.
pub(crate) fn parse_preview(text: &str) -> Option<Vec<RoutinePreviewRow>> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    let items: Vec<serde_json::Value> = serde_json::from_str(text.get(start..=end)?).ok()?;
    Some(
        items
            .into_iter()
            .filter_map(|v| {
                Some(RoutinePreviewRow {
                    thread_id: v["thread_id"].as_str()?.to_owned(),
                    bucket_id: v["bucket"].as_str().map(str::to_owned),
                    reason: v["reason"].as_str().unwrap_or_default().to_owned(),
                })
            })
            .collect(),
    )
}

/// How often the scheduler looks at the clock.
const TICK: Duration = Duration::from_secs(30);

impl Core {
    async fn start_routine_session(self: &Arc<Self>, routine: &Routine, dry: bool) -> Result<String, CoreError> {
        if routine.runner != Runner::Local && !dry {
            return Err(CoreError::new(ErrorKind::InvalidInput, "this routine runs in the cloud; publish it instead"));
        }
        let provider = routine.agent.unwrap_or(agent_api::ProviderId::ClaudeCode).as_str().to_owned();
        let session = self.clone().start_agent_session(provider, None, None).await?;
        if dry {
            self.agents.with_session(&session, |s| s.read_only = true);
        }
        let run_id = if dry {
            None
        } else {
            let db = self.db()?;
            let (rid, sid, now) = (routine.id.clone(), session.clone(), mail_sync::now_millis());
            runtime::run(async move {
                Ok::<_, CoreError>(
                    db.write(move |tx| store::start_run(tx, &rid, false, Some(&sid), "running", now)).await?,
                )
            })
            .await?
        };
        self.agents
            .routine_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.clone(), RoutineSession { routine_id: routine.id.clone(), run_id });
        let target = if dry { PromptTarget::DryRun } else { PromptTarget::Runner(Runner::Local) };
        let prompt = generate_prompt(routine, target);
        self.clone().send_agent_prompt(session.clone(), prompt, crate::agents::PromptContextInfo::default()).await?;
        self.events.emit(crate::CoreEvent::RoutinesChanged);
        Ok(session)
    }

    /// A routine session's turn ended: record the run (or the preview) and
    /// close the session.
    pub(crate) async fn routine_turn_ended(self: &Arc<Self>, session: &str, succeeded: bool) {
        let Some(state) = self.agents.routine_sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(session) else {
            return;
        };
        let Ok(db) = self.db() else { return };
        let sid = session.to_owned();
        let report = db
            .read(move |c| mail_store::agents::transcript(c, &sid))
            .await
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| serde_json::from_str::<agent_api::AgentEvent>(&r.content_json).ok())
                    .filter_map(|e| match e {
                        agent_api::AgentEvent::TextDelta { text } => Some(text),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .unwrap_or_default();
        match state.run_id {
            None => {
                let rows = parse_preview(&report).unwrap_or_default();
                self.agents.previews.lock().unwrap_or_else(|e| e.into_inner()).insert(session.to_owned(), rows);
            }
            Some(run) => {
                let routine = self.load_routine(&state.routine_id).await.ok();
                let sid = session.to_owned();
                let actions = db.read(move |c| mail_store::agents::list_actions(c, 2000)).await.unwrap_or_default();
                let mut threads: Vec<(String, Option<String>)> = Vec::new();
                let mut counts: HashMap<String, u32> = HashMap::new();
                for a in actions.iter().filter(|a| a.session_uuid == sid && a.state == "done") {
                    let args: serde_json::Value = serde_json::from_str(&a.args_json).unwrap_or_default();
                    let ids: Vec<String> = args["thread_ids"]
                        .as_array()
                        .map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
                        .unwrap_or_default();
                    if a.tool == "mail_add_label" {
                        let label = args["label"].as_str().unwrap_or_default();
                        let bucket = routine.as_ref().and_then(|r| {
                            r.buckets
                                .iter()
                                .find(|b| {
                                    r.full_label(b).eq_ignore_ascii_case(label)
                                        || b.label_name.eq_ignore_ascii_case(label)
                                })
                                .map(|b| b.id.clone())
                        });
                        for id in ids {
                            if let Some(b) = &bucket {
                                *counts.entry(b.clone()).or_default() += 1;
                            }
                            threads.retain(|(t, _)| *t != id);
                            threads.push((id, bucket.clone()));
                        }
                    } else if a.tool == "mail_archive" {
                        for id in ids {
                            if !threads.iter().any(|(t, _)| *t == id) {
                                threads.push((id, None));
                            }
                        }
                    }
                }
                let counts_json = serde_json::to_string(&counts).unwrap_or_else(|_| "{}".into());
                let status = if succeeded { "succeeded" } else { "failed" };
                let now = mail_sync::now_millis();
                let _ = db
                    .write(move |tx| {
                        store::add_run_threads(tx, run, &threads)?;
                        store::finish_run(tx, run, status, &counts_json, Some(report.trim()), now)
                    })
                    .await;
            }
        }
        let _ = self.clone().close_agent_session(session.to_owned()).await;
        self.events.emit(crate::CoreEvent::RoutinesChanged);
    }

    /// Start the scheduler for the open account (spec §11.7).
    pub(crate) fn start_routine_scheduler(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let task = runtime::runtime().spawn(async move {
            let mut last_checked: HashMap<String, i64> = HashMap::new();
            loop {
                let Some(core) = weak.upgrade() else { return };
                core.scheduler_tick(&mut last_checked, mail_sync::now_millis()).await;
                drop(core);
                tokio::time::sleep(TICK).await;
            }
        });
        if let Some(old) = self.agents.scheduler.lock().unwrap_or_else(|e| e.into_inner()).replace(task) {
            old.abort();
        }
    }

    /// One look at the clock: run local routines that came due since the
    /// last look (once, however many occurrences passed — e.g. during
    /// sleep), never overlapping a routine's own run; record a missed run
    /// for time the app was not running.
    pub(crate) async fn scheduler_tick(self: &Arc<Self>, last_checked: &mut HashMap<String, i64>, now: i64) {
        let Ok(rows) = self.list_routines().await else { return };
        for info in rows.into_iter().filter(|r| r.enabled && r.runner == "local") {
            let Ok(routine) = decode(&info.definition_json) else { continue };
            let rule = routine.schedule.rrule.clone();
            match last_checked.get(&info.id).copied() {
                None => {
                    // First look in this launch: anything due since the last
                    // run happened while the app was not running.
                    let db = match self.db() {
                        Ok(db) => db,
                        Err(_) => return,
                    };
                    let rid = info.id.clone();
                    let last_run = db
                        .read(move |c| store::runs(c, &rid, 1))
                        .await
                        .ok()
                        .and_then(|r| r.first().map(|r| r.started_at))
                        .unwrap_or(info.updated_at);
                    let missed = schedule::occurrences(&rule, last_run, now, 1000).unwrap_or_default();
                    if let Some(latest) = missed.last().copied() {
                        let rid = info.id.clone();
                        let _ = db
                            .write(move |tx| {
                                if let Some(run) = store::start_run(tx, &rid, false, None, "missed", latest)? {
                                    store::finish_run(
                                        tx,
                                        run,
                                        "missed",
                                        "{}",
                                        Some("OpenAGC was not running."),
                                        latest,
                                    )?;
                                }
                                Ok(())
                            })
                            .await;
                        self.events.emit(crate::CoreEvent::RoutinesChanged);
                    }
                    last_checked.insert(info.id.clone(), now);
                }
                Some(since) => {
                    let due = !schedule::occurrences(&rule, since, now, 1).unwrap_or_default().is_empty();
                    last_checked.insert(info.id.clone(), now);
                    let running = self
                        .agents
                        .routine_sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .values()
                        .any(|s| s.routine_id == info.id && s.run_id.is_some());
                    if due
                        && !running
                        && let Err(e) = self.start_routine_session(&routine, false).await
                    {
                        tracing::warn!(error = %e, "a scheduled routine could not start");
                    }
                }
            }
        }
    }
}

#[uniffi::export]
impl Core {
    /// Run a local routine now; returns the agent session showing it.
    pub async fn run_routine_now(self: Arc<Self>, id: String) -> Result<String, CoreError> {
        let routine = self.load_routine(&id).await?;
        self.start_routine_session(&routine, false).await
    }

    /// Classify the routine's current candidates without changing anything
    /// (a read-only agent session). Returns the session; read the result
    /// with `routine_preview` once its turn completes.
    pub async fn preview_routine(self: Arc<Self>, id: String) -> Result<String, CoreError> {
        let routine = self.load_routine(&id).await?;
        self.start_routine_session(&routine, true).await
    }

    /// A finished preview's classification (`None` while it runs).
    pub fn routine_preview(&self, session_id: String) -> Option<Vec<RoutinePreviewRow>> {
        self.agents.previews.lock().unwrap_or_else(|e| e.into_inner()).get(&session_id).cloned()
    }

    pub async fn list_routine_runs(&self, id: String, limit: u32) -> Result<Vec<RoutineRunInfo>, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let runs = db.read(move |c| store::runs(c, &id, limit)).await?;
            let mut out = Vec::with_capacity(runs.len());
            for r in runs {
                let run = r.id;
                let threads = db.read(move |c| store::run_threads(c, run)).await?;
                out.push(RoutineRunInfo {
                    run_id: r.id,
                    inferred: r.inferred,
                    session_id: r.session_uuid,
                    started_at: r.started_at,
                    ended_at: r.ended_at,
                    status: r.status,
                    counts_json: r.counts_json,
                    report_text: r.report_text,
                    thread_count: threads.len() as u32,
                });
            }
            Ok(out)
        })
        .await
    }

    /// What a schedule means, in words.
    pub fn describe_schedule(&self, rrule: String) -> String {
        schedule::describe(&rrule)
    }

    /// The next time a schedule fires after now, if within a year.
    pub fn next_run_at(&self, rrule: String) -> Option<i64> {
        schedule::next_after(&rrule, mail_sync::now_millis()).ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::executor::block_on;

    use crate::{Core, CoreConfig, CoreEvent, EventListener};

    struct Noop;
    impl EventListener for Noop {
        fn on_event(&self, _: CoreEvent) {}
    }

    #[test]
    fn routines_are_created_from_the_template_edited_and_deleted() {
        let dir = std::env::temp_dir().join(format!("openagc-core-routines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::new(
            CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(Noop),
        )
        .unwrap();
        block_on(core.clone().open_account("demo".into())).unwrap();
        let made = block_on(core.create_routine_from_template("local".into())).unwrap();
        assert_eq!(made.runner, "local");
        let mut r = super::decode(&made.definition_json).unwrap();
        assert_eq!(r.identity.primary_email, "me@example.com");
        r.name = "Sort mail".into();
        r.buckets.truncate(2);
        let saved = block_on(core.save_routine(serde_json::to_string(&r).unwrap())).unwrap();
        assert_eq!(saved.name, "Sort mail");
        assert_eq!(block_on(core.list_routines()).unwrap().len(), 1);

        r.buckets.clear();
        let err = block_on(core.save_routine(serde_json::to_string(&r).unwrap())).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::InvalidInput);
        assert_eq!(
            block_on(core.create_routine_from_template("fax".into())).unwrap_err().kind(),
            crate::ErrorKind::InvalidInput
        );

        block_on(core.delete_routine(made.id)).unwrap();
        assert!(block_on(core.list_routines()).unwrap().is_empty());
    }

    fn demo(name: &str) -> Arc<Core> {
        let dir = std::env::temp_dir().join(format!("openagc-core-routine-runs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::new(
            CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(Noop),
        )
        .unwrap();
        core.debug_use_fake_agents();
        block_on(core.clone().open_account("demo".into())).unwrap();
        block_on(core.debug_seed_demo_mailbox(40)).unwrap();
        core
    }

    fn wait_for(mut done: impl FnMut() -> bool) {
        for _ in 0..300 {
            if done() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("timed out");
    }

    #[test]
    fn previews_parse_leniently() {
        let rows = super::parse_preview(
            "Here you go:\n```json\n[{\"thread_id\":\"t1\",\"bucket\":\"daily\",\"reason\":\"alert\"},\
             {\"thread_id\":\"t2\",\"bucket\":null,\"reason\":\"a person\"},{\"bucket\":\"x\"}]\n```",
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].bucket_id.as_deref(), Some("daily"));
        assert_eq!(rows[1].bucket_id, None);
        assert!(super::parse_preview("no json here").is_none());
    }

    #[test]
    fn a_local_run_is_an_agent_session_and_is_recorded() {
        let core = demo("run");
        let routine = block_on(core.create_routine_from_template("local".into())).unwrap();
        let session = block_on(core.clone().run_routine_now(routine.id.clone())).unwrap();
        wait_for(|| {
            block_on(core.list_routine_runs(routine.id.clone(), 5))
                .unwrap()
                .first()
                .is_some_and(|r| r.status == "succeeded")
        });
        let run = block_on(core.list_routine_runs(routine.id.clone(), 5)).unwrap().remove(0);
        assert_eq!(run.session_id.as_deref(), Some(session.as_str()));
        assert!(!run.inferred);
        assert!(run.report_text.unwrap().starts_with("You said: This is an automated, unattended run."));
        wait_for(|| !core.agents.has(&session));

        let cloud = block_on(core.create_routine_from_template("claude_cloud".into())).unwrap();
        assert_eq!(
            block_on(core.clone().run_routine_now(cloud.id)).unwrap_err().kind(),
            crate::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn previews_are_read_only_and_leave_no_run() {
        let core = demo("preview");
        let routine = block_on(core.create_routine_from_template("local".into())).unwrap();
        let session = block_on(core.clone().preview_routine(routine.id.clone())).unwrap();
        wait_for(|| core.routine_preview(session.clone()).is_some());
        assert_eq!(core.routine_preview(session).unwrap(), vec![], "the scripted agent does not answer in JSON");
        assert!(block_on(core.list_routine_runs(routine.id, 5)).unwrap().is_empty());

        // A read-only session cannot change anything.
        core.agents.register("ro", permissions::Scope::Mailbox, None);
        core.agents.with_session("ro", |s| s.read_only = true);
        let archive = crate::runtime::runtime().block_on(crate::agents::tools_call_for_tests(
            &core,
            "ro",
            permissions::Tool::Archive,
            serde_json::json!({ "thread_ids": ["t"] }),
        ));
        assert!(matches!(archive, agent_mcp::Outcome::Error { code, .. } if code == "denied"));
    }

    #[test]
    fn the_scheduler_runs_due_routines_once_and_records_missed_ones() {
        let core = demo("scheduler");
        let made = block_on(core.create_routine_from_template("local".into())).unwrap();
        let mut routine = super::decode(&made.definition_json).unwrap();
        routine.schedule.rrule = "FREQ=HOURLY;BYMINUTE=0".into();
        block_on(core.save_routine(serde_json::to_string(&routine).unwrap())).unwrap();
        let now = mail_sync::now_millis();
        let hour = 3_600_000;

        // First look, three hours on: the app was not running meanwhile.
        let mut checked = std::collections::HashMap::new();
        crate::runtime::runtime().block_on(core.scheduler_tick(&mut checked, now + 3 * hour));
        let runs = block_on(core.list_routine_runs(made.id.clone(), 10)).unwrap();
        assert_eq!(runs.len(), 1, "one missed record however many were missed");
        assert_eq!(runs[0].status, "missed");

        // Nothing due within the same minute.
        crate::runtime::runtime().block_on(core.scheduler_tick(&mut checked, now + 3 * hour + 1000));
        assert_eq!(block_on(core.list_routine_runs(made.id.clone(), 10)).unwrap().len(), 1);

        // An hour later (or after sleeping through several): one run.
        crate::runtime::runtime().block_on(core.scheduler_tick(&mut checked, now + 6 * hour));
        wait_for(|| block_on(core.list_routine_runs(made.id.clone(), 10)).unwrap().len() == 2);
        let runs = block_on(core.list_routine_runs(made.id.clone(), 10)).unwrap();
        assert!(runs.iter().any(|r| r.session_id.is_some()));
    }
}
