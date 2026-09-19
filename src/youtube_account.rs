//! Public channel metadata and cleanup of grants saved by older Brick versions.
//! New channel sharing never requests or refreshes a Google OAuth token.
use crate::credential_store::Store;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

static STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub channel_id: String,
    pub title: String,
    pub url: String,
}

// Deserialize only to revoke an old grant during explicitly requested erasure.
#[derive(Deserialize)]
struct Session {
    version: u32,
    account: String,
    access_token: String,
    refresh_token: Option<String>,
}

/// Remove obsolete local credentials without exercising the Google grant.
/// Remote revocation affects the whole Google project and remains user initiated.
pub fn forget_legacy_connection(account: &str) -> Result<(), String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Unlock protected storage to remove the old YouTube connection.")?;
    Store::youtube(account)?
        .remove_if_present()
        .map_err(storage_error)
}

/// Only the explicitly confirmed account-erasure flow calls this. Google
/// revocation affects this user's grants across the OAuth project's clients;
/// ordinary browser sign-out and local Forget account must not call it.
pub(crate) fn revoke_for_erasure(account: &str) -> Result<(), String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Unlock protected storage to finish deletion.")?;
    let store = Store::youtube(account)?;
    let Some(bytes) = store.load().map_err(storage_error)? else {
        return Ok(());
    };
    let session: Session = serde_json::from_slice(&bytes)
        .map_err(|_| "Couldn't read the YouTube connection for deletion.")?;
    if session.version != 1
        || session.account != account
        || !credential(&session.access_token)
        || session
            .refresh_token
            .as_ref()
            .is_some_and(|token| !credential(token))
    {
        return Err("Couldn't verify the YouTube connection for deletion.".into());
    }
    let token = session
        .refresh_token
        .as_deref()
        .unwrap_or(&session.access_token);
    crate::account_erasure::revoke_token(
        "https://oauth2.googleapis.com/revoke",
        &[("token", token)],
        true,
    )?;
    store.remove().map_err(storage_error)
}

fn credential(value: &str) -> bool {
    !value.is_empty() && value.len() <= 8192 && value.bytes().all(|b| b.is_ascii_graphic())
}

fn storage_error(message: String) -> String {
    message.replace("Warcraft Logs", "YouTube")
}
