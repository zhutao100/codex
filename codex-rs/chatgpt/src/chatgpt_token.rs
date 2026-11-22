use codex_core::AuthManager;
use std::path::Path;

use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::token_data::TokenData;

/// Resolve one request's identity; anonymous and API-key callers have no directory token.
pub(crate) async fn load_chatgpt_token(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<Option<TokenData>> {
    let auth_manager =
        AuthManager::new(codex_home.to_path_buf(), false, auth_credentials_store_mode);
    auth_manager
        .auth()
        .await
        .filter(codex_core::CodexAuth::is_chatgpt_auth)
        .map(|auth| auth.get_token_data())
        .transpose()
}
