//! Data lifecycle management for login/logout scenarios.
//!
//! These FRB-exposed functions handle:
//! - Claiming anonymous data after login (attributing user_id)
//! - Wiping all local data on logout

use crate::storage::Storage;

/// Called after a successful login to attribute all anonymous (user_id=NULL)
/// local data to the newly authenticated user.
///
/// This should be called from Flutter after login/signup succeeds.
pub fn on_login_claim_data(db_path: String, user_id: String) -> Result<u64, String> {
    let storage = Storage::new(&db_path).map_err(|e| e.to_string())?;
    let claimed = storage
        .claim_anonymous_data(&user_id)
        .map_err(|e| e.to_string())?;
    Ok(claimed as u64)
}

/// Called after logout to wipe all local user data, resetting the database
/// to a clean state.
///
/// This should be called from Flutter after logout succeeds.
pub fn on_logout_wipe_data(db_path: String) -> Result<(), String> {
    let storage = Storage::new(&db_path).map_err(|e| e.to_string())?;
    storage.wipe_all_data().map_err(|e| e.to_string())
}
