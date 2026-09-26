//! Claude cloud routines through the user's CLI (spec §11.5), and the paste
//! hand-off when that is not possible.

use agent_api::routines::{PromptTarget, Routine, Runner, generate_prompt, prompt_fingerprint, schedule};
use agent_claude::routines::{self as cloud, CloudError, CloudRoutines};
use mail_store::routines as store;

use crate::routines::RoutineInfo;
use crate::{Core, CoreError, ErrorKind, runtime};

impl From<CloudError> for CoreError {
    fn from(e: CloudError) -> Self {
        let kind = match e {
            CloudError::NotInstalled => ErrorKind::NotFound,
            _ => ErrorKind::Agent,
        };
        CoreError::new(kind, e.to_string())
    }
}

/// Everything the user needs to create the routine by hand.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RoutineHandoff {
    pub prompt: String,
    /// For Claude: the UTC cron to paste.
    pub cron_utc: Option<String>,
    pub schedule_text: String,
    /// The page to open.
    pub url: String,
}

fn iso_ms(text: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text?).ok().map(|d| d.timestamp_millis())
}

impl Core {
    fn cloud(&self) -> CloudRoutines {
        match self.agents.cloud_locator.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(locator) => CloudRoutines::new(&locator),
            None => CloudRoutines::standard(),
        }
    }

    fn cloud_prompt(routine: &Routine) -> String {
        generate_prompt(routine, PromptTarget::Runner(Runner::ClaudeCloud))
    }
}

#[uniffi::export]
impl Core {
    /// Create or update the routine at claude.ai through the user's CLI.
    /// Errors mean: use the paste hand-off.
    pub async fn publish_routine_to_cloud(&self, id: String) -> Result<RoutineInfo, CoreError> {
        let mut routine = self.load_routine(&id).await?;
        if routine.runner != Runner::ClaudeCloud {
            return Err(CoreError::new(ErrorKind::InvalidInput, "this routine does not run on Claude cloud"));
        }
        let cron =
            schedule::to_utc_cron(&routine.schedule.rrule).map_err(|e| CoreError::new(ErrorKind::InvalidInput, e))?;
        let prompt = Self::cloud_prompt(&routine);
        let client = self.cloud();
        let cloud_state = routine.cloud.clone();
        let (name, enabled) = (routine.name.clone(), routine.enabled);
        let published = runtime::run(async move {
            match cloud_state.trigger_id {
                Some(trigger) => {
                    // Only what the editor owns; the connector and
                    // environment stay as the user set them at claude.ai.
                    let partial = serde_json::json!({
                        "name": name,
                        "cron_expression": cron,
                        "enabled": enabled,
                        "job_config": { "ccr": { "events": [{ "data": {
                            "uuid": format!("openagc-{}", mail_sync::now_millis()),
                            "session_id": "", "type": "user", "parent_tool_use_id": null,
                            "message": { "role": "user", "content": prompt },
                        } }] } },
                    });
                    client.update(&trigger, &partial).await?;
                    Ok::<_, CoreError>((trigger.clone(), cloud::routine_url(&trigger), cloud_state.environment_id))
                }
                None => {
                    let setup = client.discover().await?;
                    let body = cloud::create_body(
                        &name,
                        &cron,
                        enabled,
                        &prompt,
                        &setup,
                        &format!("openagc-{}", mail_sync::now_millis()),
                    )?;
                    let created = client.create(&body).await?;
                    Ok((created.trigger_id, created.url, setup.environment_id))
                }
            }
        })
        .await?;
        routine.cloud.trigger_id = Some(published.0);
        routine.cloud.routine_url = Some(published.1);
        routine.cloud.environment_id = published.2;
        routine.cloud.published_fingerprint = Some(prompt_fingerprint(&Self::cloud_prompt(&routine)));
        routine.cloud.published_at = Some(mail_sync::now_millis());
        self.store_routine(&routine).await
    }

    /// Turn a routine on or off (at claude.ai too, for a published one).
    pub async fn set_routine_enabled(&self, id: String, enabled: bool) -> Result<RoutineInfo, CoreError> {
        let mut routine = self.load_routine(&id).await?;
        routine.enabled = enabled;
        if let (Runner::ClaudeCloud, Some(trigger)) = (routine.runner, routine.cloud.trigger_id.clone()) {
            let client = self.cloud();
            runtime::run(async move {
                Ok::<_, CoreError>(client.update(&trigger, &serde_json::json!({ "enabled": enabled })).await?)
            })
            .await?;
        }
        self.store_routine(&routine).await
    }

    /// Fire a published cloud routine now.
    pub async fn run_cloud_routine_now(&self, id: String) -> Result<Option<String>, CoreError> {
        let routine = self.load_routine(&id).await?;
        let trigger = routine
            .cloud
            .trigger_id
            .ok_or_else(|| CoreError::new(ErrorKind::InvalidInput, "publish the routine first"))?;
        let client = self.cloud();
        let session = runtime::run(async move { Ok::<_, CoreError>(client.run(&trigger).await?) }).await?;
        if let (Some(session), Ok(db)) = (session.clone(), self.db()) {
            let (rid, now) = (id.clone(), mail_sync::now_millis());
            let _ = runtime::run(async move {
                Ok::<_, CoreError>(
                    db.write(move |tx| store::upsert_cloud_run(tx, &rid, &session, "running", now, None)).await?,
                )
            })
            .await;
        }
        self.account_events().emit(crate::CoreEvent::RoutinesChanged);
        Ok(session)
    }

    /// Bring the routine's cloud runs (and the newest reports) into the run
    /// history. Logs are stored as plain text and never given to an agent.
    pub async fn refresh_cloud_runs(&self, id: String) -> Result<(), CoreError> {
        let routine = self.load_routine(&id).await?;
        let Some(trigger) = routine.cloud.trigger_id else { return Ok(()) };
        let client = self.cloud();
        let db = self.db()?;
        runtime::run(async move {
            let runs = client.list_runs(&trigger).await?;
            // Each log is one CLI call on the user's account: only the few
            // newest finished runs that have none yet.
            let mut logs_left = 3;
            for run in runs.iter().take(20) {
                let (rid, sid, status) = (id.clone(), run.session_id.clone(), run.status.clone());
                let started = iso_ms(run.started_at.as_deref()).unwrap_or_else(mail_sync::now_millis);
                let ended = iso_ms(run.ended_at.as_deref());
                let row = db.write(move |tx| store::upsert_cloud_run(tx, &rid, &sid, &status, started, ended)).await?;
                // Inferred runs inside this run's window were this run.
                if let Some(row) = row {
                    let (rid, end) = (id.clone(), ended.unwrap_or(started) + 5 * 60 * 1000);
                    let inferred =
                        db.read(move |c| store::inferred_runs_between(c, &rid, started - 60_000, end)).await?;
                    for from in inferred {
                        db.write(move |tx| store::merge_runs(tx, from, row)).await?;
                    }
                }
                let finished = matches!(run.status.as_str(), "completed" | "succeeded" | "failed");
                let Some(row) = row else { continue };
                if !finished || logs_left == 0 || db.read(move |c| store::run_report(c, row)).await?.is_some() {
                    continue;
                }
                logs_left -= 1;
                if let Ok(log) = client.run_log(&run.session_id).await {
                    db.write(move |tx| store::set_report(tx, row, &log)).await?;
                }
            }
            Ok::<_, CoreError>(())
        })
        .await?;
        self.account_events().emit(crate::CoreEvent::RoutinesChanged);
        Ok(())
    }

    /// The paste hand-off (spec §11.5): prompt, schedule and the page to
    /// create the routine on, for when publishing through the CLI fails or
    /// the runner has no API (ChatGPT).
    pub async fn routine_handoff(&self, id: String) -> Result<RoutineHandoff, CoreError> {
        let routine = self.load_routine(&id).await?;
        let prompt = generate_prompt(&routine, PromptTarget::Runner(routine.runner));
        let schedule_text = schedule::describe(&routine.schedule.rrule);
        Ok(match routine.runner {
            Runner::ChatGptCloud => {
                RoutineHandoff { prompt, cron_utc: None, schedule_text, url: "https://chatgpt.com".into() }
            }
            _ => RoutineHandoff {
                prompt,
                cron_utc: schedule::to_utc_cron(&routine.schedule.rrule).ok(),
                schedule_text,
                url: cloud::ROUTINES_PAGE.into(),
            },
        })
    }

    /// The user created the routine by hand and pasted its URL (or id).
    pub async fn attach_cloud_routine(&self, id: String, url_or_id: String) -> Result<RoutineInfo, CoreError> {
        let trigger = cloud::trigger_id_from(&url_or_id).ok_or_else(|| {
            CoreError::new(ErrorKind::InvalidInput, "that does not look like a routine link (…/routines/trig_…)")
        })?;
        let mut routine = self.load_routine(&id).await?;
        routine.cloud.routine_url = Some(cloud::routine_url(&trigger));
        routine.cloud.trigger_id = Some(trigger);
        routine.cloud.published_fingerprint = Some(prompt_fingerprint(&Self::cloud_prompt(&routine)));
        routine.cloud.published_at = Some(mail_sync::now_millis());
        self.store_routine(&routine).await
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use agent_api::process::Locator;
    use futures::executor::block_on;

    use crate::{Core, CoreConfig, CoreEvent, EventListener};

    struct Noop;
    impl EventListener for Noop {
        fn on_event(&self, _: Option<String>, _: CoreEvent) {}
    }

    /// A core whose cloud calls reach a fake `claude`, never the real one.
    fn core_with_fake_claude(name: &str) -> (Arc<Core>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("openagc-core-cloud-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-claude/tests/fake_claude_routines.py");
        std::fs::copy(fake, bin.join("claude")).unwrap();
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let core = Core::new(
            CoreConfig { data_dir: dir.join("data").to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(Noop),
        )
        .unwrap();
        *core.agents.cloud_locator.lock().unwrap() = Some(Locator::only(vec![bin.clone()]));
        block_on(core.clone().open_account("demo".into())).unwrap();
        (core, bin)
    }

    #[test]
    fn publish_update_toggle_run_and_history() {
        let (core, bin) = core_with_fake_claude("flow");
        let made = block_on(core.create_routine_from_template("claude_cloud".into())).unwrap();
        let published = block_on(core.publish_routine_to_cloud(made.id.clone())).unwrap();
        assert_eq!(published.cloud_url.as_deref(), Some("https://claude.ai/code/routines/trig_01NEW"));
        assert!(!published.changed_since_publish);
        let body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(bin.join("created.json")).unwrap()).unwrap();
        let prompt = body["job_config"]["ccr"]["events"][0]["data"]["message"]["content"].as_str().unwrap();
        assert!(prompt.contains("label_thread"), "the Claude connector's tool map");
        assert_eq!(body["job_config"]["ccr"]["environment_id"], "env_42");

        // An edit shows as unpublished until published again (an update).
        let mut routine = crate::routines::decode(&published.definition_json).unwrap();
        routine.buckets[0].description.push_str(" Pager alerts too.");
        let edited = block_on(core.save_routine(serde_json::to_string(&routine).unwrap())).unwrap();
        assert!(edited.changed_since_publish);
        let again = block_on(core.publish_routine_to_cloud(made.id.clone())).unwrap();
        assert!(!again.changed_since_publish);
        assert!(bin.join("updated.json").exists());

        let off = block_on(core.set_routine_enabled(made.id.clone(), false)).unwrap();
        assert!(!off.enabled);
        let update: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(bin.join("updated.json")).unwrap()).unwrap();
        assert_eq!(update["body"], serde_json::json!({ "enabled": false }));

        assert_eq!(block_on(core.run_cloud_routine_now(made.id.clone())).unwrap().as_deref(), Some("session_run_1"));
        block_on(core.refresh_cloud_runs(made.id.clone())).unwrap();
        let runs = block_on(core.list_routine_runs(made.id.clone(), 10)).unwrap();
        assert_eq!(runs.len(), 1, "the run started here and the listed run are the same record");
        assert_eq!(runs[0].status, "completed");
        assert!(runs[0].report_text.as_deref().unwrap().starts_with("Sorted 4 threads"));
    }

    #[test]
    fn the_paste_hand_off_and_attaching_a_url() {
        let (core, _) = core_with_fake_claude("handoff");
        let made = block_on(core.create_routine_from_template("claude_cloud".into())).unwrap();
        let handoff = block_on(core.routine_handoff(made.id.clone())).unwrap();
        assert_eq!(handoff.url, "https://claude.ai/code/routines");
        assert!(handoff.cron_utc.is_some());
        assert!(handoff.prompt.contains("This is an automated, unattended run"));
        assert_eq!(handoff.schedule_text, "Every hour at :44");

        let err = block_on(core.attach_cloud_routine(made.id.clone(), "https://example.com/x".into())).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::InvalidInput);
        let attached =
            block_on(core.attach_cloud_routine(made.id.clone(), "https://claude.ai/code/routines/trig_PASTED".into()))
                .unwrap();
        assert_eq!(attached.cloud_url.as_deref(), Some("https://claude.ai/code/routines/trig_PASTED"));

        let chatgpt = block_on(core.create_routine_from_template("chat_gpt_cloud".into())).unwrap();
        let h = block_on(core.routine_handoff(chatgpt.id.clone())).unwrap();
        assert_eq!((h.url.as_str(), h.cron_utc), ("https://chatgpt.com", None));
        assert!(block_on(core.publish_routine_to_cloud(chatgpt.id)).is_err(), "no API for ChatGPT");
    }

    #[test]
    fn fake_agent_mode_never_reaches_a_real_cli() {
        let dir = std::env::temp_dir().join(format!("openagc-core-cloud-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::new(
            CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(Noop),
        )
        .unwrap();
        core.debug_use_fake_agents();
        block_on(core.clone().open_account("demo".into())).unwrap();
        let made = block_on(core.create_routine_from_template("claude_cloud".into())).unwrap();
        let err = block_on(core.publish_routine_to_cloud(made.id)).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::NotFound, "no claude found: {err:?}");
    }
}
