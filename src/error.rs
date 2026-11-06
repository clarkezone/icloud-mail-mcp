use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ConfigurationError,
    AuthenticationFailed,
    TlsError,
    UpstreamUnavailable,
    MailboxNotFound,
    MessageNotFound,
    StaleReference,
    CursorExpired,
    InvalidCursor,
    UnsupportedSearch,
    UnsupportedContent,
    SizeLimit,
    Busy,
    DeadlineExceeded,
    Cancelled,
    InternalError,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailError(pub ErrorCode);
pub type Result<T> = std::result::Result<T, MailError>;
impl MailError {
    pub fn retryable(self) -> bool {
        matches!(
            self.0,
            ErrorCode::Busy | ErrorCode::UpstreamUnavailable | ErrorCode::DeadlineExceeded
        )
    }
    pub fn value(self) -> serde_json::Value {
        serde_json::json!({"error": {"code": self.0, "retryable": self.retryable(),
            "retry_after_ms": if self.retryable() { Some(1000) } else { None }}})
    }
}
impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mail operation failed ({:?})", self.0)
    }
}
impl std::error::Error for MailError {}
impl From<std::io::Error> for MailError {
    fn from(_: std::io::Error) -> Self {
        Self(ErrorCode::UpstreamUnavailable)
    }
}
