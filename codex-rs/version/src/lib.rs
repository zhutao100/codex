/// The current Codex version, preferring `CODEX_VERSION_OVERRIDE` if set at compile time.
pub const CODEX_VERSION: &str = match option_env!("CODEX_VERSION_OVERRIDE") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
