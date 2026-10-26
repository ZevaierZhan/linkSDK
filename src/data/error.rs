use serde::Serialize;

/// Stable machine-readable categories. Never infer retry policy from Display text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum DataErrorKind {
    Busy,
    Locked,
    StaleVersion,
    CapacityExceeded,
    DiskFull,
    ReadOnly,
    CorruptStorage,
    UnsupportedFormat,
    SpaceMismatch,
    InvalidInput,
    Io,
    StorageFailure,
    Cancelled,
    DeadlineExceeded,
}
#[derive(Debug, Clone, Serialize)]
pub struct DataError {
    pub kind: DataErrorKind,
    pub sqlite_code: Option<i32>,
    message: String,
}
impl DataError {
    pub fn new(kind: DataErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            sqlite_code: None,
            message: message.into(),
        }
    }
    /// Only lock contention is automatically retried. CAS conflicts require a new read.
    pub fn is_retryable(&self) -> bool {
        matches!(self.kind, DataErrorKind::Busy | DataErrorKind::Locked)
    }
    pub(crate) fn sqlite(e: &rusqlite::Error) -> Self {
        use rusqlite::ErrorCode::*;
        if let rusqlite::Error::SqliteFailure(code, _) = e {
            let kind = match code.code {
                DatabaseBusy => DataErrorKind::Busy,
                DatabaseLocked => DataErrorKind::Locked,
                DiskFull => DataErrorKind::DiskFull,
                ReadOnly => DataErrorKind::ReadOnly,
                DatabaseCorrupt | NotADatabase => DataErrorKind::CorruptStorage,
                SystemIoFailure | CannotOpen => DataErrorKind::Io,
                _ => DataErrorKind::StorageFailure,
            };
            Self {
                kind,
                sqlite_code: Some(code.extended_code),
                message: e.to_string(),
            }
        } else {
            Self::new(DataErrorKind::StorageFailure, e.to_string())
        }
    }
    pub(crate) fn corrupt(e: impl std::fmt::Display) -> Self {
        Self::new(DataErrorKind::CorruptStorage, e.to_string())
    }
}
impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for DataError {}
impl From<&str> for DataError {
    fn from(s: &str) -> Self {
        Self::new(DataErrorKind::InvalidInput, s)
    }
}
impl From<String> for DataError {
    fn from(s: String) -> Self {
        Self::new(DataErrorKind::InvalidInput, s)
    }
}
impl From<DataError> for String {
    fn from(e: DataError) -> Self {
        e.to_string()
    }
}
pub type DataResult<T> = Result<T, DataError>;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_readonly_and_table_lock_errors_keep_distinct_codes() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE x(n); INSERT INTO x VALUES(1)")
            .unwrap();
        let mut q = db.prepare("SELECT n FROM x").unwrap();
        let mut rows = q.query([]).unwrap();
        assert!(rows.next().unwrap().is_some());
        let locked = DataError::sqlite(&db.execute("DROP TABLE x", []).unwrap_err());
        assert_eq!(locked.kind, DataErrorKind::Locked);
        assert!(locked.is_retryable());
        drop(rows);
        drop(q);
        db.pragma_update(None, "query_only", true).unwrap();
        let readonly = DataError::sqlite(&db.execute("INSERT INTO x VALUES(2)", []).unwrap_err());
        assert_eq!(readonly.kind, DataErrorKind::ReadOnly);
        assert!(!readonly.is_retryable());
    }
}
