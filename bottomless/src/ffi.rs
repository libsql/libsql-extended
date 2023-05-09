pub use sqld_libsql_bindings::ffi::{
    libsql_wal, libsql_wal_methods, sqlite3, sqlite3_file, sqlite3_vfs, PageHdrIter, PgHdr, Wal,
    WalIndexHdr, SQLITE_CANTOPEN, SQLITE_CHECKPOINT_TRUNCATE, SQLITE_IOERR_READ,
    SQLITE_IOERR_WRITE, SQLITE_OK, SQLITE_READONLY,
};

#[repr(C)]
pub struct bottomless_methods {
    pub methods: libsql_wal_methods,
    pub underlying_methods: *const libsql_wal_methods,
}
