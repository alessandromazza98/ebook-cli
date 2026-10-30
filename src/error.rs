use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Error {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip)]
    pub exit: i32,
}
pub type Result<T> = std::result::Result<T, Error>;
impl Error {
    pub fn new(code: &str, message: impl Into<String>, exit: i32) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            exit,
        }
    }
    pub fn dependency(operation: &str) -> Self {
        Self {
            code: "dependency_unavailable".into(),
            message: format!(
                "{operation} failed; check credentials, connectivity and upstream status"
            ),
            retryable: true,
            exit: 4,
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message, 2)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new("conflict", message, 5)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new(
            "io_error",
            format!("Local file operation failed ({:?})", e.kind()),
            7,
        )
    }
}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::new(
            "database_error",
            "SQLite operation failed; check state directory permissions, disk capacity and schema",
            7,
        )
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::invalid("Invalid JSON or incompatible schema")
    }
}
