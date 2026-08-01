use tauri::ipc::InvokeError;

pub const SYNC_OPERATION_FAILED: &str = "Sync operation failed; see the application log";
pub const INVALID_SYNC_INPUT: &str = "Invalid sync input";

#[derive(Debug, Clone)]
pub struct Error(pub String);
pub type Result<T> = std::result::Result<T, Error>;

impl From<Error> for InvokeError {
    fn from(err: Error) -> Self {
        Self(serde_json::Value::String(err.0))
    }
}

impl<E: std::error::Error> From<E> for Error {
    fn from(e: E) -> Self {
        Self(e.to_string())
    }
}

impl From<Error> for anyhow::Error {
    fn from(err: Error) -> Self {
        anyhow::Error::msg(err.0)
    }
}

pub fn stable_sync_error(message: &'static str) -> Error {
    Error(message.to_string())
}
