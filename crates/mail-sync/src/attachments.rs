//! Attachment bytes on demand (spec §14.3, §15.3). Nothing is downloaded
//! until the user opens, previews or drags an attachment, or an inline
//! image is shown. Files are cached under the data directory, one folder
//! per message, so a second open is instant and offline.

use std::path::{Path, PathBuf};

use mail_store::{Db, StoreError, read};
use provider_api::MailProvider;

use crate::error::SyncResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentFile {
    pub path: PathBuf,
    pub filename: String,
    pub mime_type: String,
    pub content_id: Option<String>,
    /// False when the file was already cached.
    pub downloaded: bool,
}

/// The cached file for attachment `id` (its row id), fetching it first if
/// needed. `provider` is `None` for accounts with no server (the demo):
/// only bytes stored with the message are available then.
pub async fn attachment_file(
    db: &Db,
    provider: Option<&dyn MailProvider>,
    cache_dir: &Path,
    id: i64,
) -> SyncResult<AttachmentFile> {
    let source = db
        .read(move |c| read::attachment_source(c, id))
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("attachment {id}")))?;
    let filename = safe_filename(&source.filename);
    let folder = cache_dir
        .join(safe_filename(source.message_id.as_str()))
        .join(safe_filename(source.part_id.as_deref().unwrap_or("0")));
    let path = folder.join(&filename);
    let file = |downloaded| AttachmentFile {
        path: path.clone(),
        filename: filename.clone(),
        mime_type: source.mime_type.clone(),
        content_id: source.content_id.clone(),
        downloaded,
    };
    if path.is_file() {
        return Ok(file(false));
    }
    let bytes = match (&source.data, &source.provider_attachment_id, provider) {
        (Some(data), _, _) => data.clone(),
        (None, Some(attachment_id), Some(provider)) => {
            provider.fetch_attachment(&source.message_id, attachment_id).await?
        }
        (None, _, _) => {
            return Err(StoreError::NotFound(format!("{} is not available offline", source.filename)).into());
        }
    };
    std::fs::create_dir_all(&folder).map_err(|e| StoreError::Io(e.to_string()))?;
    // Write then rename, so a half-written file is never mistaken for the
    // cached copy.
    let partial = folder.join(format!(".{filename}.partial"));
    std::fs::write(&partial, &bytes).map_err(|e| StoreError::Io(e.to_string()))?;
    std::fs::rename(&partial, &path).map_err(|e| StoreError::Io(e.to_string()))?;
    Ok(file(true))
}

/// A name that stays inside its folder: no separators, no leading dots,
/// no control characters, not empty, not absurdly long.
pub fn safe_filename(name: &str) -> String {
    let cleaned: String =
        name.chars().map(|c| if matches!(c, '/' | '\\' | ':') || c.is_control() { '_' } else { c }).collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    let mut out: String = if trimmed.is_empty() { "attachment".into() } else { trimmed.to_owned() };
    if out.len() > 200 {
        // Keep the extension when shortening.
        let ext = Path::new(&out).extension().and_then(|e| e.to_str()).map(str::to_owned);
        let mut cut = 180;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        if let Some(ext) = ext.filter(|e| e.len() < 16) {
            out = format!("{out}.{ext}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::safe_filename;

    #[test]
    fn filenames_cannot_escape_the_cache() {
        assert_eq!(safe_filename("report.pdf"), "report.pdf");
        assert_eq!(safe_filename("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_filename("..hidden"), "hidden");
        assert_eq!(safe_filename("a\u{0}b\nc"), "a_b_c");
        assert_eq!(safe_filename("   "), "attachment");
        assert_eq!(safe_filename(".."), "attachment");
        let long = format!("{}.pdf", "é".repeat(150));
        let short = safe_filename(&long);
        assert!(short.len() <= 200 && short.ends_with(".pdf"), "{short}");
    }
}
