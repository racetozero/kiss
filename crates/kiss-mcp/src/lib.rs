//! First-class Model Context Protocol support for KISS.

pub mod config;
pub mod manager;
pub mod oauth;
pub mod tool;

pub use config::{
    AuthMode, AuthSetting, ConfigPaths, ConfigScope, LoadedConfig, McpConfig, OAuthConfig,
    OAuthGrantType, ServerEntry,
};
pub use manager::{
    CachedPrompt, CachedResource, CachedTool, McpManager, ServerState, ServerStatus,
    probe_oauth_challenge,
};
pub use oauth::{
    PendingLogin, begin_login, finish_login, has_credentials, login_client_credentials, logout,
};
pub use tool::McpTool;

fn ensure_tls_crypto_provider() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
    });
}
