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
                IndexEntry { id, kind: AccountKind::Gmail, email, display_name: None, avatar_file: None, added_at },
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
        let db = tokio::task::spawn_blocking(move || Db::open(&path))
            .await
            .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))??;
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
        let _guard = self.index_lock.lock().await;
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
                    }
                })
                .collect())
        })
        .await
    }

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
            if changed {
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
        if self.current_account_id().as_deref() == Some(account_id.as_str()) {
            self.stop_sync();
        }
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
        fn on_event(&self, _event: crate::CoreEvent) {}
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
        }))
        .unwrap();
        let list = block_on(core.list_accounts()).unwrap();
        assert_eq!(list.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["one", "two", "three"]);
        assert_eq!(list[1].display_name.as_deref(), Some("Two"));
        assert_eq!(list.iter().map(|a| a.position).collect::<Vec<_>>(), [0, 1, 2]);

        block_on(core.move_account("three".into(), 0)).unwrap();
        let order: Vec<String> = block_on(core.list_accounts()).unwrap().into_iter().map(|a| a.id).collect();
        assert_eq!(order, ["three", "one", "two"]);

        // Switching opens stores; both stay open, one is current.
        block_on(core.clone().set_current_account("one".into())).unwrap();
        block_on(core.clone().set_current_account("two".into())).unwrap();
        assert_eq!(core.current_account_id().as_deref(), Some("two"));
        assert_eq!(core.open_accounts.read().unwrap().stores.len(), 2);

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
}
