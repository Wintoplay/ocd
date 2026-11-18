//! Storage failures (`core/src/database/storage-failure.ts`, opencode 2.3.1): SQLite result codes that mean storage
//! itself can't be used, as opposed to a bad statement or a constraint, each with a message that doesn't name the
//! database engine.

/// The readable message for SQLite primary result `code` (an extended code keeps its primary code in the low byte).
/// BUSY and LOCKED only surface once a write has already waited out `busy_timeout`, so the write is lost just like
/// on a full disk.
pub fn message_for_code(code: i32) -> Option<&'static str> {
    Some(match code & 0xff {
        5 | 6 => "Storage is in use by another process", // SQLITE_BUSY, SQLITE_LOCKED
        8 => "Storage is read-only",                      // SQLITE_READONLY
        10 => "Storage couldn't be read or written",      // SQLITE_IOERR
        11 | 26 => "Storage is damaged",                  // SQLITE_CORRUPT, SQLITE_NOTADB
        13 => "Storage is full",                          // SQLITE_FULL
        14 => "Storage couldn't be opened",               // SQLITE_CANTOPEN
        _ => return None,
    })
}

/// The (extended) result code of a SQLite failure.
pub fn result_code(error: &rusqlite::Error) -> Option<i32> {
    match error {
        rusqlite::Error::SqliteFailure(e, _) => Some(e.extended_code),
        _ => None,
    }
}

/// `StorageFailure.fromSqlError`: the readable message for a SQLite error the driver reported as a storage failure.
pub fn from_sqlite(error: &rusqlite::Error) -> Option<&'static str> {
    result_code(error).and_then(message_for_code)
}

/// `StorageFailure.message`: the readable message for the storage failure that failed an operation, found anywhere
/// in its error chain, if that is what failed it.
pub fn message(error: &anyhow::Error) -> Option<&'static str> {
    error.chain().filter_map(|e| e.downcast_ref::<rusqlite::Error>()).find_map(from_sqlite)
}

/// `StorageFailure.rolledBack`: whether SQLite may have already rolled back the transaction this error failed
/// (SQLITE_FULL, SQLITE_IOERR, SQLITE_BUSY, SQLITE_NOMEM, SQLITE_INTERRUPT), so a rollback has nothing to undo.
pub fn rolled_back(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|e| e.downcast_ref::<rusqlite::Error>())
        .filter_map(result_code)
        .any(|code| matches!(code & 0xff, 5 | 7 | 9 | 10 | 13))
}
