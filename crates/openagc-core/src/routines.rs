//! Routines across the FFI (spec §11). The definition crosses as JSON (the
//! `agent_api::routines::Routine` shape); Swift decodes it into its own
//! Codable mirror for the editor.

use agent_api::routines::{Identity, Routine, Runner};
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
}
