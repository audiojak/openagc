//! The accounts on this Mac (spec §7.7): an index file listing them in the
//! user's order, the stores currently open, and which one the window shows.
//!
//! `accounts/index.json` is the list; each account's own directory holds
//! its store. The index is rebuilt by scanning the directories when it is
//! missing (an install from before multiple accounts), using the address
//! each store records once it has synced.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mail_store::Db;
use serde::{Deserialize, Serialize};

use crate::{Core, CoreError, ErrorKind, runtime};

tokio::task_local! {
    /// The account that work on this task belongs to, when that is not the
    /// window's current account: an agent session's tool calls, a
    /// background routine run. Every store and event lookup honours it, so
    /// such work can never read or change another account's mail
    /// (spec §7.7). `runtime::run` carries it onto the core runtime.
    pub(crate) static SCOPED_ACCOUNT: String;
}

/// The account scoped to this task, if any.
pub(crate) fn scoped_account() -> Option<String> {
    SCOPED_ACCOUNT.try_with(Clone::clone).ok()
}

/// Run `fut` scoped to `account` (or unscoped when `None`).
pub(crate) async fn scoped<F: std::future::Future>(account: Option<String>, fut: F) -> F::Output {
    match account {
        Some(account) => SCOPED_ACCOUNT.scope(account, fut).await,
        None => fut.await,
    }
}

/// The demo mailbox has a directory but is not a user account.
pub(crate) const DEMO_ACCOUNT_ID: &str = "demo";
const INDEX_FILE: &str = "index.json";

/// What an account is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    /// A Gmail account, synced.
    Gmail,
    /// An imported mailbox with no server (spec §7.8).
    Archive,
}

/// One account as listed behind the avatar button.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AccountSummary {
    pub id: String,
    pub kind: AccountKind,
    /// The Gmail address, or the archive's name.
    pub email: String,
    pub display_name: Option<String>,
    /// Absolute path of the cached avatar image, if any.
    pub avatar_path: Option<String>,
    pub position: u32,
    /// Unread threads in the Inbox, for the avatar menu and the Dock.
    pub inbox_unread: u32,
    /// Backfill may use IMAP (full mail access was granted).
    pub imap_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndexEntry {
    pub id: String,
    pub kind: AccountKind,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// File name inside the account directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_file: Option<String>,
    #[serde(default)]
    pub added_at: i64,
    /// Full mail access was granted, so backfill may use IMAP. `None`
    /// leaves an existing value alone when re-registering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap: Option<bool>,
}

/// An account directory no listed account owns.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct OrphanedStore {
    pub id: String,
    /// The Gmail address it synced, if it got that far.
    pub email: Option<String>,
    pub bytes: u64,
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => dir_size(&e.path()),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}

/// Stores that are open, and the current one.
#[derive(Default)]
pub(crate) struct OpenAccounts {
    pub stores: HashMap<String, Db>,
    pub current: Option<String>,
}

pub(crate) fn accounts_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("accounts")
}

/// Read the index, rebuilding it from the account directories if it is
/// missing or unreadable. Blocking.
pub(crate) fn load_index(data_dir: &Path) -> Vec<IndexEntry> {
    let path = accounts_dir(data_dir).join(INDEX_FILE);
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(entries) = serde_json::from_slice::<Vec<IndexEntry>>(&bytes)
    {
        return entries;
    }
    let scanned = scan(data_dir);
    if !scanned.is_empty() {
        tracing::info!(accounts = scanned.len(), "rebuilt the accounts index from the account directories");
        let _ = save_index(data_dir, &scanned);
    }
    scanned
}

/// Write the index atomically. Blocking.
pub(crate) fn save_index(data_dir: &Path, entries: &[IndexEntry]) -> std::io::Result<()> {
    let dir = accounts_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{INDEX_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(entries).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, dir.join(INDEX_FILE))
}

/// Accounts found on disk: Gmail stores that know their address, and
/// archives (which carry an `account.json`). Sorted by directory age.
fn scan(data_dir: &Path) -> Vec<IndexEntry> {
    let Ok(entries) = std::fs::read_dir(accounts_dir(data_dir)) else { return Vec::new() };
    let mut found: Vec<(std::time::SystemTime, IndexEntry)> = Vec::new();
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.path();
        if id == DEMO_ACCOUNT_ID || !dir.is_dir() || !crate::mail::valid_account_id(&id) {
            continue;
        }
        let created =
            entry.metadata().and_then(|m| m.created().or_else(|_| m.modified())).unwrap_or(std::time::UNIX_EPOCH);
        let added_at = created.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        if let Some(archive) = read_archive_meta(&dir) {
            found.push((
                created,
                IndexEntry {
                    id,
                    kind: AccountKind::Archive,
                    email: archive.name,
                    display_name: None,
                    avatar_file: None,
                    added_at,
                    imap: None,
                },
            ));
            continue;
        }
        let store = dir.join("mail.sqlite");
        if !store.is_file() {
            continue;
        }
        let email = Db::open(&store)
            .ok()
            .and_then(|db| db.read_blocking(|c| mail_store::read::sync_state(c, "account_email")).ok().flatten());
        if let Some(email) = email {
            found.push((
                created,
                IndexEntry {
                    id,
                    kind: AccountKind::Gmail,
                    email,
                    display_name: None,
                    avatar_file: None,
                    added_at,
                    imap: None,
                },
            ));
        }
    }
    found.sort_by_key(|(created, _)| *created);
    found.into_iter().map(|(_, e)| e).collect()
}

#[derive(Deserialize)]
struct ArchiveMetaName {
    name: String,
}

fn read_archive_meta(dir: &Path) -> Option<ArchiveMetaName> {
    let bytes = std::fs::read(dir.join("account.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

impl Core {
    /// The account work on this task acts on: the task's scoped account,
    /// else the window's current one.
    pub(crate) fn effective_account_id(&self) -> Option<String> {
        scoped_account().or_else(|| self.current_account_id())
    }

    /// Events tagged with the effective account.
    pub(crate) fn account_events(&self) -> crate::EventBus {
        self.events.for_account(self.effective_account_id())
    }

    pub(crate) fn data_path(&self) -> PathBuf {
        PathBuf::from(&self.config.data_dir)
    }

    /// Record an account in the index (new or updated), keeping its place.
    pub(crate) async fn register_account(&self, entry: IndexEntry) -> Result<(), CoreError> {
        let data_dir = self.data_path();
        let _guard = self.index_lock.lock().await;
        runtime::run(async move {
            tokio::task::spawn_blocking(move || {
                let mut entries = load_index(&data_dir);
                match entries.iter_mut().find(|e| e.id == entry.id) {
                    Some(existing) => {
                        existing.email = entry.email;
                        existing.kind = entry.kind;
                        if entry.display_name.is_some() {
                            existing.display_name = entry.display_name;
                        }
                        if entry.avatar_file.is_some() {
                            existing.avatar_file = entry.avatar_file;
                        }
                        if entry.imap.is_some() {
                            existing.imap = entry.imap;
                        }
                    }
                    None => entries.push(entry),
                }
                save_index(&data_dir, &entries)
            })
            .await
            .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?
            .map_err(|e| CoreError::new(ErrorKind::Storage, e.to_string()))
        })
        .await
    }

    /// The store for an account, opening it if needed. Does not change the
    /// current account.
    pub(crate) async fn store_for(&self, account_id: &str) -> Result<Db, CoreError> {
        if !crate::mail::valid_account_id(account_id) {
            return Err(CoreError::new(ErrorKind::InvalidInput, "account id must be 1-64 of [A-Za-z0-9-]"));
        }
        if let Some(db) = self.open_accounts.read().unwrap_or_else(|e| e.into_inner()).stores.get(account_id) {
            return Ok(db.clone());
        }
        let path = self.account_db_path(account_id);
        let db = runtime::run(async move {
            tokio::task::spawn_blocking(move || Db::open(&path))
                .await
                .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?
                .map_err(CoreError::from)
        })
        .await?;
        tracing::info!(account = %account_id, "account opened");
        let mut open = self.open_accounts.write().unwrap_or_else(|e| e.into_inner());
        // Another caller may have opened it meanwhile; keep the first.
        Ok(open.stores.entry(account_id.to_owned()).or_insert(db).clone())
    }

    /// Close an account's store and forget it (removal).
    pub(crate) fn close_store(&self, account_id: &str) {
        let mut open = self.open_accounts.write().unwrap_or_else(|e| e.into_inner());
        if let Some(db) = open.stores.remove(account_id) {
            db.close();
        }
        if open.current.as_deref() == Some(account_id) {
            open.current = None;
        }
    }
}

#[uniffi::export]
impl Core {
    /// The user's accounts in their order (the demo mailbox is not one).
    pub async fn list_accounts(&self) -> Result<Vec<AccountSummary>, CoreError> {
        let data_dir = self.data_path();
        let mut accounts = {
            let _guard = self.index_lock.lock().await;
            self.list_index(data_dir).await?
        };
        for account in &mut accounts {
            if let Ok(db) = self.store_for(&account.id).await {
                account.inbox_unread =
                    runtime::run(async move { Ok(db.read(mail_store::read::list_mailboxes).await?) })
                        .await
                        .ok()
                        .and_then(|boxes| boxes.into_iter().find(|m| mail_store::read::mailbox_label(m) == "INBOX"))
                        .map(|m| m.unread_count)
                        .unwrap_or(0);
            }
        }
        Ok(accounts)
    }
}

impl Core {
    async fn list_index(&self, data_dir: PathBuf) -> Result<Vec<AccountSummary>, CoreError> {
        runtime::run(async move {
            let entries = tokio::task::spawn_blocking({
                let data_dir = data_dir.clone();
                move || load_index(&data_dir)
            })
            .await
            .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?;
            Ok(entries
                .into_iter()
                .enumerate()
                .map(|(i, e)| {
                    let avatar_path = e
                        .avatar_file
                        .as_ref()
                        .map(|f| accounts_dir(&data_dir).join(&e.id).join(f))
                        .filter(|p| p.is_file())
                        .map(|p| p.to_string_lossy().into_owned());
                    AccountSummary {
                        id: e.id,
                        kind: e.kind,
                        email: e.email,
                        display_name: e.display_name,
                        avatar_path,
                        position: i as u32,
                        inbox_unread: 0,
                        imap_enabled: e.imap.unwrap_or(false),
                    }
                })
                .collect())
        })
        .await
    }
}

#[uniffi::export]
impl Core {
    /// Show `account_id` in the window: open its store if needed and make
    /// it current. Idempotent.
    pub async fn set_current_account(self: Arc<Self>, account_id: String) -> Result<(), CoreError> {
        let core = self.clone();
        runtime::run(async move {
            core.store_for(&account_id).await?;
            let changed = {
                let mut open = core.open_accounts.write().unwrap_or_else(|e| e.into_inner());
                let changed = open.current.as_deref() != Some(account_id.as_str());
                open.current = Some(account_id);
                changed
            };
            // One scheduler serves every account; start it once.
            if changed && core.agents.scheduler.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
                core.start_routine_scheduler();
            }
            Ok(())
        })
        .await
    }

    /// Remove an account from this Mac: stop its sync, forget its
    /// credentials, delete its store and cached files, and drop it from the
    /// index. Gmail itself is not touched.
    pub async fn remove_account(&self, account_id: String) -> Result<(), CoreError> {
        if !crate::mail::valid_account_id(&account_id) || account_id == DEMO_ACCOUNT_ID {
            return Err(CoreError::new(ErrorKind::InvalidInput, "not a removable account"));
        }
        self.stop_sync_for(&account_id);
        self.close_store(&account_id);
        self.secrets.delete(crate::secrets::keys::refresh_token(&account_id))?;
        self.secrets.delete(crate::account::client_key(&account_id))?;
        let data_dir = self.data_path();
        let _guard = self.index_lock.lock().await;
        runtime::run(async move {
            let dir = accounts_dir(&data_dir).join(&account_id);
            let _ = tokio::fs::remove_dir_all(&dir).await;
            tokio::task::spawn_blocking(move || {
                let mut entries = load_index(&data_dir);
                entries.retain(|e| e.id != account_id);
                save_index(&data_dir, &entries)
            })
            .await
            .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?
            .map_err(|e| CoreError::new(ErrorKind::Storage, e.to_string()))
        })
        .await
    }

    /// Development and test hook: add a listed account holding a synthetic
    /// mailbox of `threads` threads, with no sign-in (so it never syncs).
    /// Snapshots and UI tests use it to show several accounts without
    /// touching Google.
    pub async fn debug_add_demo_account(
        self: Arc<Self>,
        account_id: String,
        email: String,
        display_name: Option<String>,
        threads: u32,
    ) -> Result<(), CoreError> {
        let db = self.store_for(&account_id).await?;
        let seed_email = email.clone();
        runtime::run(async move {
            let spec = mail_store::demo::DemoSpec { threads, ..Default::default() };
            tokio::task::spawn_blocking(move || -> Result<(), CoreError> {
                mail_store::demo::generate(&db, &spec)?;
                db.write_blocking(move |tx| mail_store::read::set_sync_state(tx, "account_email", &seed_email))?;
                Ok(())
            })
            .await
            .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?
        })
        .await?;
        self.register_account(IndexEntry {
            id: account_id,
            kind: AccountKind::Gmail,
            email,
            display_name,
            avatar_file: None,
            added_at: mail_sync::now_millis(),
            imap: None,
        })
        .await
    }

    /// Stop using IMAP for an account's backfill (turning it on means
    /// signing in again with full mail access). Takes effect when its sync
    /// next starts.
    pub async fn disable_imap(&self, account_id: String) -> Result<(), CoreError> {
        let data_dir = self.data_path();
        let entry = {
            let _guard = self.index_lock.lock().await;
            runtime::run(async move {
                tokio::task::spawn_blocking(move || load_index(&data_dir))
                    .await
                    .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))
            })
            .await?
            .into_iter()
            .find(|e| e.id == account_id)
        };
        let Some(mut entry) = entry else { return Err(CoreError::new(ErrorKind::NotFound, "no such account")) };
        entry.imap = Some(false);
        self.register_account(entry).await
    }

    /// Account directories that no listed account owns (left behind by an
    /// older "Sign In Again", or a crash mid-removal). The demo is not one.
    pub async fn orphaned_stores(&self) -> Result<Vec<OrphanedStore>, CoreError> {
        let data_dir = self.data_path();
        let _guard = self.index_lock.lock().await;
        runtime::run(async move {
            tokio::task::spawn_blocking(move || {
                let listed: std::collections::HashSet<String> =
                    load_index(&data_dir).into_iter().map(|e| e.id).collect();
                let Ok(entries) = std::fs::read_dir(accounts_dir(&data_dir)) else { return Ok(Vec::new()) };
                let mut out = Vec::new();
                for entry in entries.flatten() {
                    let id = entry.file_name().to_string_lossy().into_owned();
                    let dir = entry.path();
                    if !dir.is_dir()
                        || id == DEMO_ACCOUNT_ID
                        || listed.contains(&id)
                        || !crate::mail::valid_account_id(&id)
                    {
                        continue;
                    }
                    let email = Db::open(&dir.join("mail.sqlite")).ok().and_then(|db| {
                        db.read_blocking(|c| mail_store::read::sync_state(c, "account_email")).ok().flatten()
                    });
                    out.push(OrphanedStore { id, email, bytes: dir_size(&dir) });
                }
                out.sort_by(|a, b| a.id.cmp(&b.id));
                Ok(out)
            })
            .await
            .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?
        })
        .await
    }

    /// Delete an orphaned store. Refuses anything a listed account owns.
    pub async fn remove_orphaned_store(&self, account_id: String) -> Result<(), CoreError> {
        let orphans = self.orphaned_stores().await?;
        if !orphans.iter().any(|o| o.id == account_id) {
            return Err(CoreError::new(ErrorKind::InvalidInput, "that store belongs to an account"));
        }
        self.close_store(&account_id);
        let _ = self.secrets.delete(crate::secrets::keys::refresh_token(&account_id));
        let _ = self.secrets.delete(crate::account::client_key(&account_id));
        let dir = accounts_dir(&self.data_path()).join(&account_id);
        runtime::run(async move {
            tokio::fs::remove_dir_all(dir).await.map_err(|e| CoreError::new(ErrorKind::Storage, e.to_string()))
        })
        .await
    }

    /// Move an account to `position` in the list (the avatar menu order and
    /// the ⌃1–⌃9 shortcuts).
    pub async fn move_account(&self, account_id: String, position: u32) -> Result<(), CoreError> {
        let data_dir = self.data_path();
        let _guard = self.index_lock.lock().await;
        runtime::run(async move {
            tokio::task::spawn_blocking(move || {
                let mut entries = load_index(&data_dir);
                let Some(from) = entries.iter().position(|e| e.id == account_id) else {
                    return Err(CoreError::new(ErrorKind::NotFound, "no such account"));
                };
                let entry = entries.remove(from);
                entries.insert((position as usize).min(entries.len()), entry);
                save_index(&data_dir, &entries).map_err(|e| CoreError::new(ErrorKind::Storage, e.to_string()))
            })
            .await
            .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRoot(PathBuf);
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn temp(name: &str) -> TempRoot {
        let root = std::env::temp_dir().join(format!("openagc-registry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        TempRoot(root)
    }

    fn gmail_store(root: &Path, id: &str, email: Option<&str>) {
        let path = accounts_dir(root).join(id).join("mail.sqlite");
        let db = Db::open(&path).unwrap();
        if let Some(email) = email {
            let email = email.to_owned();
            db.write_blocking(move |tx| mail_store::read::set_sync_state(tx, "account_email", &email)).unwrap();
        }
        db.close();
    }

    #[test]
    fn a_missing_index_is_rebuilt_from_the_account_directories() {
        let t = temp("scan");
        gmail_store(&t.0, "aaa", Some("first@example.com"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        gmail_store(&t.0, "bbb", Some("second@example.com"));
        gmail_store(&t.0, "ccc", None); // never synced: nothing to list
        gmail_store(&t.0, DEMO_ACCOUNT_ID, Some("demo@example.com"));
        std::fs::create_dir_all(accounts_dir(&t.0).join("arc")).unwrap();
        std::fs::write(accounts_dir(&t.0).join("arc/account.json"), br#"{"kind":"archive","name":"Old mail"}"#)
            .unwrap();

        let entries = load_index(&t.0);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "{ids:?}");
        assert!(ids.iter().position(|i| *i == "aaa") < ids.iter().position(|i| *i == "bbb"), "oldest first");
        let arc = entries.iter().find(|e| e.id == "arc").unwrap();
        assert_eq!(arc.kind, AccountKind::Archive);
        assert_eq!(arc.email, "Old mail");
        assert!(accounts_dir(&t.0).join(INDEX_FILE).is_file(), "the rebuilt index is saved");

        // Once saved, the index is the truth: a new directory is not listed
        // until it is registered.
        gmail_store(&t.0, "ddd", Some("fourth@example.com"));
        assert_eq!(load_index(&t.0).len(), 3);
    }

    #[test]
    fn a_corrupt_index_is_rebuilt() {
        let t = temp("corrupt");
        gmail_store(&t.0, "aaa", Some("a@example.com"));
        std::fs::write(accounts_dir(&t.0).join(INDEX_FILE), b"{not json").unwrap();
        assert_eq!(load_index(&t.0).iter().map(|e| e.email.as_str()).collect::<Vec<_>>(), ["a@example.com"]);
    }

    #[derive(Default)]
    struct NoEvents;
    impl crate::EventListener for NoEvents {
        fn on_event(&self, _account: Option<String>, _event: crate::CoreEvent) {}
    }

    #[test]
    fn accounts_are_registered_listed_reordered_switched_and_removed() {
        use crate::SecretStore;
        use futures::executor::block_on;
        let t = temp("core");
        let secrets = Arc::new(crate::secrets::MemorySecrets::default());
        let core = Core::new(
            crate::CoreConfig { data_dir: t.0.to_string_lossy().into_owned(), log_dir: None },
            secrets.clone(),
            Arc::new(NoEvents),
        )
        .unwrap();
        assert!(block_on(core.list_accounts()).unwrap().is_empty());
        for (id, email) in [("one", "one@example.com"), ("two", "two@example.com"), ("three", "three@example.com")] {
            block_on(core.register_account(IndexEntry {
                id: id.into(),
                kind: AccountKind::Gmail,
                email: email.into(),
                display_name: None,
                avatar_file: None,
                added_at: 0,
                imap: None,
            }))
            .unwrap();
        }
        // Re-registering updates in place.
        block_on(core.register_account(IndexEntry {
            id: "two".into(),
            kind: AccountKind::Gmail,
            email: "two@example.com".into(),
            display_name: Some("Two".into()),
            avatar_file: None,
            added_at: 0,
            imap: None,
        }))
        .unwrap();
        let list = block_on(core.list_accounts()).unwrap();
        assert_eq!(list.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["one", "two", "three"]);
        assert_eq!(list[1].display_name.as_deref(), Some("Two"));
        assert_eq!(list.iter().map(|a| a.position).collect::<Vec<_>>(), [0, 1, 2]);

        // The IMAP grant is recorded, kept by later registrations that do
        // not mention it, and can be turned off.
        let mut granted = IndexEntry {
            id: "one".into(),
            kind: AccountKind::Gmail,
            email: "one@example.com".into(),
            display_name: None,
            avatar_file: None,
            added_at: 0,
            imap: Some(true),
        };
        block_on(core.register_account(granted.clone())).unwrap();
        granted.imap = None;
        block_on(core.register_account(granted)).unwrap();
        let imap = |core: &Arc<Core>| {
            block_on(core.list_accounts()).unwrap().iter().find(|a| a.id == "one").unwrap().imap_enabled
        };
        assert!(imap(&core));
        block_on(core.disable_imap("one".into())).unwrap();
        assert!(!imap(&core));
        assert!(block_on(core.disable_imap("nobody".into())).is_err());

        block_on(core.move_account("three".into(), 0)).unwrap();
        let order: Vec<String> = block_on(core.list_accounts()).unwrap().into_iter().map(|a| a.id).collect();
        assert_eq!(order, ["three", "one", "two"]);

        // Switching opens stores; both stay open, one is current.
        block_on(core.clone().set_current_account("one".into())).unwrap();
        block_on(core.clone().set_current_account("two".into())).unwrap();
        assert_eq!(core.current_account_id().as_deref(), Some("two"));
        let open: Vec<String> = core.open_accounts.read().unwrap().stores.keys().cloned().collect();
        assert!(open.contains(&"one".to_owned()) && open.contains(&"two".to_owned()), "{open:?}");

        // Removal deletes the store, the credentials and the entry.
        secrets.set(crate::secrets::keys::refresh_token("two"), "token".into()).unwrap();
        block_on(core.remove_account("two".into())).unwrap();
        assert_eq!(core.current_account_id(), None, "the current account was removed");
        assert!(!accounts_dir(&t.0).join("two").exists());
        assert_eq!(secrets.get(crate::secrets::keys::refresh_token("two")).unwrap(), None);
        let ids: Vec<String> = block_on(core.list_accounts()).unwrap().into_iter().map(|a| a.id).collect();
        assert_eq!(ids, ["three", "one"]);
        assert!(block_on(core.remove_account(DEMO_ACCOUNT_ID.into())).is_err(), "the demo is not an account");
    }

    #[derive(Default)]
    struct Tags(std::sync::Mutex<Vec<(Option<String>, String)>>);
    impl crate::EventListener for Tags {
        fn on_event(&self, account: Option<String>, event: crate::CoreEvent) {
            if let crate::CoreEvent::ThreadsChanged { mailbox_id, .. } = event {
                self.0.lock().unwrap().push((account, mailbox_id));
            }
        }
    }

    #[test]
    fn work_scoped_to_an_account_stays_on_it_whatever_the_window_shows() {
        use futures::executor::block_on;
        let t = temp("scope");
        let tags = Arc::new(Tags::default());
        let core = Core::new(
            crate::CoreConfig { data_dir: t.0.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            tags.clone(),
        )
        .unwrap();
        block_on(core.clone().set_current_account("alpha".into())).unwrap();
        block_on(core.debug_seed_demo_mailbox(20)).unwrap();
        block_on(core.clone().set_current_account("beta".into())).unwrap();

        async fn inbox(core: &Arc<Core>) -> u32 {
            let boxes = core.list_mailboxes().await.unwrap();
            boxes.into_iter().find(|m| m.id == "INBOX").map(|m| m.total_count).unwrap_or(0)
        }
        let alpha = || Some("alpha".to_owned());
        assert_eq!(block_on(inbox(&core)), 0, "the window shows beta, which is empty");
        let scoped_inbox = block_on(scoped(alpha(), inbox(&core)));
        assert!(scoped_inbox > 0, "scoped work reads alpha");

        // A change made under alpha's scope lands in alpha and says so.
        let thread = block_on(scoped(alpha(), core.list_threads("INBOX".into(), None, 1))).unwrap().rows.remove(0).id;
        block_on(scoped(alpha(), core.archive(vec![thread]))).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let seen = tags.0.lock().unwrap().clone();
        assert!(seen.iter().any(|(a, m)| a.as_deref() == Some("alpha") && m == "INBOX"), "{seen:?}");
        assert!(seen.iter().all(|(a, _)| a.as_deref() != Some("beta")), "nothing was tagged for the window: {seen:?}");
        assert_eq!(block_on(scoped(alpha(), inbox(&core))), scoped_inbox - 1);
        assert_eq!(block_on(inbox(&core)), 0, "beta untouched");
    }

    #[test]
    fn a_composer_keeps_writing_to_the_account_it_was_opened_on() {
        use futures::executor::block_on;
        let t = temp("composer");
        let core = Core::new(
            crate::CoreConfig { data_dir: t.0.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(NoEvents),
        )
        .unwrap();
        block_on(core.clone().set_current_account("alpha".into())).unwrap();
        let composer = core.clone().composer_for("alpha".into());
        // The user switches the window to beta while the composer is open.
        block_on(core.clone().set_current_account("beta".into())).unwrap();
        let draft = crate::DraftInfo {
            id: 0,
            thread_id: None,
            in_reply_to_message_id: None,
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: "From alpha".into(),
            body_html: "<p>hi</p>".into(),
            quoted_html: String::new(),
            attachments: vec![],
            status: crate::DraftStatus::Editing,
            error: None,
            updated_at: 0,
        };
        let id = block_on(composer.save_draft(draft)).unwrap();
        assert!(block_on(core.list_drafts()).unwrap().is_empty(), "nothing landed in beta");
        let in_alpha = block_on(scoped(Some("alpha".into()), core.list_drafts())).unwrap();
        assert_eq!(in_alpha.iter().map(|d| d.id).collect::<Vec<_>>(), [id]);
        assert_eq!(block_on(composer.get_draft(id)).unwrap().unwrap().subject, "From alpha");
        block_on(composer.delete_draft(id)).unwrap();
        assert!(block_on(scoped(Some("alpha".into()), core.list_drafts())).unwrap().is_empty());
    }

    #[test]
    fn orphaned_stores_are_found_and_only_they_can_be_removed() {
        use futures::executor::block_on;
        let t = temp("orphans");
        gmail_store(&t.0, "listed", Some("me@example.com"));
        let core = Core::new(
            crate::CoreConfig { data_dir: t.0.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(NoEvents),
        )
        .unwrap();
        assert_eq!(block_on(core.list_accounts()).unwrap().len(), 1, "the index is built with the listed store");
        gmail_store(&t.0, "left-behind", Some("me@example.com"));
        gmail_store(&t.0, DEMO_ACCOUNT_ID, None);
        let orphans = block_on(core.orphaned_stores()).unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].id, "left-behind");
        assert_eq!(orphans[0].email.as_deref(), Some("me@example.com"));
        assert!(orphans[0].bytes > 0);
        assert!(block_on(core.remove_orphaned_store("listed".into())).is_err(), "never a listed account");
        assert!(block_on(core.remove_orphaned_store(DEMO_ACCOUNT_ID.into())).is_err());
        block_on(core.remove_orphaned_store("left-behind".into())).unwrap();
        assert!(!accounts_dir(&t.0).join("left-behind").exists());
        assert!(accounts_dir(&t.0).join("listed").exists());
        assert!(block_on(core.orphaned_stores()).unwrap().is_empty());
    }
}
