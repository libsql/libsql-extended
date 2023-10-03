use std::ffi::{c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use rusqlite::{DatabaseName, ErrorCode, OpenFlags, StatementStatus};
use sqld_libsql_bindings::wal_hook::{TransparentMethods, WalMethodsHook};
use tokio::sync::{watch, Notify};
use tokio::time::{Duration, Instant};

use crate::auth::{Authenticated, Authorized, Permission};
use crate::error::Error;
use crate::libsql_bindings::wal_hook::WalHook;
use crate::query::Query;
use crate::query_analysis::{State, StmtKind};
use crate::query_result_builder::{QueryBuilderConfig, QueryResultBuilder};
use crate::replication::FrameNo;
use crate::stats::Stats;
use crate::Result;

use super::config::DatabaseConfigStore;
use super::program::{Cond, DescribeCol, DescribeParam, DescribeResponse, DescribeResult};
use super::{MakeConnection, Program, Step, TXN_TIMEOUT};

pub struct MakeLibSqlConn<W: WalHook + 'static> {
    db_path: PathBuf,
    hook: &'static WalMethodsHook<W>,
    ctx_builder: Box<dyn Fn() -> W::Context + Sync + Send + 'static>,
    stats: Arc<Stats>,
    config_store: Arc<DatabaseConfigStore>,
    extensions: Arc<[PathBuf]>,
    max_response_size: u64,
    max_total_response_size: u64,
    auto_checkpoint: u32,
    current_frame_no_receiver: watch::Receiver<Option<FrameNo>>,
    state: Arc<TxnState<W>>,
    /// In wal mode, closing the last database takes time, and causes other databases creation to
    /// return sqlite busy. To mitigate that, we hold on to one connection
    _db: Option<LibSqlConnection<W>>,
}

impl<W: WalHook + 'static> MakeLibSqlConn<W>
where
    W: WalHook + 'static + Sync + Send,
    W::Context: Send + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub async fn new<F>(
        db_path: PathBuf,
        hook: &'static WalMethodsHook<W>,
        ctx_builder: F,
        stats: Arc<Stats>,
        config_store: Arc<DatabaseConfigStore>,
        extensions: Arc<[PathBuf]>,
        max_response_size: u64,
        max_total_response_size: u64,
        auto_checkpoint: u32,
        current_frame_no_receiver: watch::Receiver<Option<FrameNo>>,
    ) -> Result<Self>
    where
        F: Fn() -> W::Context + Sync + Send + 'static,
    {
        let mut this = Self {
            db_path,
            hook,
            ctx_builder: Box::new(ctx_builder),
            stats,
            config_store,
            extensions,
            max_response_size,
            max_total_response_size,
            auto_checkpoint,
            current_frame_no_receiver,
            _db: None,
            state: Default::default(),
        };

        let db = this.try_create_db().await?;
        this._db = Some(db);

        Ok(this)
    }

    /// Tries to create a database, retrying if the database is busy.
    async fn try_create_db(&self) -> Result<LibSqlConnection<W>> {
        // try 100 times to acquire initial db connection.
        let mut retries = 0;
        loop {
            match self.make_connection().await {
                Ok(conn) => return Ok(conn),
                Err(
                    err @ Error::RusqliteError(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error {
                            code: ErrorCode::DatabaseBusy,
                            ..
                        },
                        _,
                    )),
                ) => {
                    if retries < 100 {
                        tracing::warn!("Database file is busy, retrying...");
                        retries += 1;
                        tokio::time::sleep(Duration::from_millis(100)).await
                    } else {
                        Err(err)?;
                    }
                }
                Err(e) => Err(e)?,
            }
        }
    }

    async fn make_connection(&self) -> Result<LibSqlConnection<W>> {
        LibSqlConnection::new(
            self.db_path.clone(),
            self.extensions.clone(),
            self.hook,
            (self.ctx_builder)(),
            self.stats.clone(),
            self.config_store.clone(),
            QueryBuilderConfig {
                max_size: Some(self.max_response_size),
                max_total_size: Some(self.max_total_response_size),
                auto_checkpoint: self.auto_checkpoint,
            },
            self.current_frame_no_receiver.clone(),
            self.state.clone(),
        )
        .await
    }
}

#[async_trait::async_trait]
impl<W> MakeConnection for MakeLibSqlConn<W>
where
    W: WalHook + 'static + Sync + Send,
    W::Context: Send + 'static,
{
    type Connection = LibSqlConnection<W>;

    async fn create(&self) -> Result<Self::Connection, Error> {
        self.make_connection().await
    }
}

#[derive(Clone)]
pub struct LibSqlConnection<W: WalHook> {
    inner: Arc<Mutex<Connection<W>>>,
}

pub fn open_conn<W>(
    path: &Path,
    wal_methods: &'static WalMethodsHook<W>,
    hook_ctx: W::Context,
    flags: Option<OpenFlags>,
    auto_checkpoint: u32,
) -> Result<sqld_libsql_bindings::Connection<W>, rusqlite::Error>
where
    W: WalHook,
{
    let flags = flags.unwrap_or(
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    );
    sqld_libsql_bindings::Connection::open(path, flags, wal_methods, hook_ctx, auto_checkpoint)
}

impl<W> LibSqlConnection<W>
where
    W: WalHook,
    W::Context: Send,
{
    pub async fn new(
        path: impl AsRef<Path> + Send + 'static,
        extensions: Arc<[PathBuf]>,
        wal_hook: &'static WalMethodsHook<W>,
        hook_ctx: W::Context,
        stats: Arc<Stats>,
        config_store: Arc<DatabaseConfigStore>,
        builder_config: QueryBuilderConfig,
        current_frame_no_receiver: watch::Receiver<Option<FrameNo>>,
        state: Arc<TxnState<W>>,
    ) -> crate::Result<Self> {
        let conn = tokio::task::spawn_blocking(move || {
            Connection::new(
                path.as_ref(),
                extensions,
                wal_hook,
                hook_ctx,
                stats,
                config_store,
                builder_config,
                current_frame_no_receiver,
                state,
            )
        })
        .await
        .unwrap()?;

        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }
}

struct Connection<W: WalHook = TransparentMethods> {
    conn: sqld_libsql_bindings::Connection<W>,
    stats: Arc<Stats>,
    config_store: Arc<DatabaseConfigStore>,
    builder_config: QueryBuilderConfig,
    current_frame_no_receiver: watch::Receiver<Option<FrameNo>>,
    // must be dropped after the connection because the connection refers to it
    state: Arc<TxnState<W>>,
    // current txn slot if any
    slot: Option<Arc<TxnSlot<W>>>,
}

/// A slot for holding the state of a transaction lock permit
struct TxnSlot<T: WalHook> {
    /// Pointer to the connection holding the lock. Used to rollback the transaction when the lock
    /// is stolen.
    conn: Arc<Mutex<Connection<T>>>,
    /// Time at which the transaction can be stolen
    timeout_at: tokio::time::Instant,
    /// The transaction lock was stolen
    is_stolen: AtomicBool,
}

/// The transaction state shared among all connections to the same database
pub struct TxnState<T: WalHook> {
    /// Slot for the connection currently holding the transaction lock
    slot: RwLock<Option<Arc<TxnSlot<T>>>>,
    /// Notifier for when the lock gets dropped
    notify: Notify,
}

impl<W: WalHook> Default for TxnState<W> {
    fn default() -> Self {
        Self {
            slot: Default::default(),
            notify: Default::default(),
        }
    }
}

/// The lock-stealing busy handler.
/// Here is a detailed description of the algorithm:
/// - all connections to a database share a `TxnState`, that contains a `TxnSlot`
/// - when a connection acquire a write lock to the database, this is detected by monitoring the state of the
///   connection before and after the call thanks to [sqlite3_txn_state()](https://www.sqlite.org/c3ref/c_txn_none.html)
/// - if the connection acquired a write lock (txn state none/read -> write), a new txn slot is created. A clone of the
///   `TxnSlot` is placed in the `TxnState` shared with other connections to this database, while another clone is kept in
///   the transaction state. The TxnSlot contains: the instant at which the txn should timeout, a `is_stolen` flag, and a
///   pointer to the connection currently holding the lock.
/// - when another connection attempts to acquire the lock, the `busy_handler` callback will be called. The callback is being
///   passed the `TxnState` for the connection. The handler looks at the current slot to determine when the current txn will
///   timeout, and waits for that instant before retrying. The waiting handler can also be notified that the transaction has
///   been finished early.
/// - If the handler waits until the txn timeout and isn't notified of the termination of the txn, it will attempt to steal the lock.
///   This is done by calling rollback on the slot's txn, and marking the slot as stolen.
/// - When a connection notices that it's slot has been stolen, it returns a timedout error to the next request.
unsafe extern "C" fn busy_handler<W: WalHook>(state: *mut c_void, _retries: c_int) -> c_int {
    let state = &*(state as *mut TxnState<W>);
    let lock = state.slot.read();
    // fast path
    if lock.is_none() {
        return 1;
    }

    tokio::runtime::Handle::current().block_on(async move {
        let timeout = {
            let slot = lock.as_ref().unwrap();
            let timeout_at = slot.timeout_at;
            drop(lock);
            tokio::time::sleep_until(timeout_at)
        };

        tokio::select! {
            _ = state.notify.notified() => 1,
            _ = timeout => {
                // attempt to steal the lock
                let mut lock = state.slot.write();
                // we attempt to take the slot, and steal the transaction from the other
                // connection
                if let Some(slot) = lock.take() {
                    if Instant::now() >= slot.timeout_at {
                        tracing::info!("stole transaction lock");
                        let conn = slot.conn.lock();
                        // we have a lock on the connection, we don't need mode than a
                        // Relaxed store.
                        slot.is_stolen.store(true, std::sync::atomic::Ordering::Relaxed);
                        conn.rollback();
                    }
                }
                1
            }
        }
    })
}

impl<W: WalHook> Connection<W> {
    fn new(
        path: &Path,
        extensions: Arc<[PathBuf]>,
        wal_methods: &'static WalMethodsHook<W>,
        hook_ctx: W::Context,
        stats: Arc<Stats>,
        config_store: Arc<DatabaseConfigStore>,
        builder_config: QueryBuilderConfig,
        current_frame_no_receiver: watch::Receiver<Option<FrameNo>>,
        state: Arc<TxnState<W>>,
    ) -> Result<Self> {
        let mut conn = open_conn(
            path,
            wal_methods,
            hook_ctx,
            None,
            builder_config.auto_checkpoint,
        )?;

        // register the lock-stealing busy handler
        unsafe {
            let ptr = Arc::as_ptr(&state) as *mut _;
            rusqlite::ffi::sqlite3_busy_handler(conn.handle(), Some(busy_handler::<W>), ptr);
        }

        let this = Self {
            conn,
            stats,
            config_store,
            builder_config,
            current_frame_no_receiver,
            state,
            slot: None,
        };

        for ext in extensions.iter() {
            unsafe {
                let _guard = rusqlite::LoadExtensionGuard::new(&this.conn).unwrap();
                if let Err(e) = this.conn.load_extension(ext, None) {
                    tracing::error!("failed to load extension: {}", ext.display());
                    Err(e)?;
                }
                tracing::debug!("Loaded extension {}", ext.display());
            }
        }

        Ok(this)
    }

    fn run<B: QueryResultBuilder>(
        this: Arc<Mutex<Self>>,
        pgm: Program,
        mut builder: B,
    ) -> Result<(B, State)> {
        use rusqlite::TransactionState as Tx;

        let state = this.lock().state.clone();

        let mut results = Vec::with_capacity(pgm.steps.len());
        builder.init(&this.lock().builder_config)?;
        let mut previous_state = this
            .lock()
            .conn
            .transaction_state(Some(DatabaseName::Main))?;

        let mut has_timeout = false;
        for step in pgm.steps() {
            let mut lock = this.lock();

            if let Some(slot) = &lock.slot {
                if slot.is_stolen.load(Ordering::Relaxed) || Instant::now() > slot.timeout_at {
                    lock.rollback();
                    has_timeout = true;
                }
            }

            // once there was a timeout, invalidate all the program steps
            if has_timeout {
                lock.slot = None;
                builder.begin_step()?;
                builder.step_error(Error::LibSqlTxTimeout)?;
                builder.finish_step(0, None)?;
                continue;
            }

            let res = lock.execute_step(step, &results, &mut builder)?;

            let new_state = lock.conn.transaction_state(Some(DatabaseName::Main))?;
            match (previous_state, new_state) {
                // lock was upgraded, claim the slot
                (Tx::None | Tx::Read, Tx::Write) => {
                    let slot = Arc::new(TxnSlot {
                        conn: this.clone(),
                        timeout_at: Instant::now() + TXN_TIMEOUT,
                        is_stolen: AtomicBool::new(false),
                    });

                    lock.slot.replace(slot.clone());
                    state.slot.write().replace(slot);
                }
                // lock was downgraded, notify a waiter
                (Tx::Write, Tx::None | Tx::Read) => {
                    state.slot.write().take();
                    lock.slot.take();
                    state.notify.notify_one();
                }
                // nothing to do
                (_, _) => (),
            }

            previous_state = new_state;

            results.push(res);
        }

        builder.finish(*this.lock().current_frame_no_receiver.borrow_and_update())?;

        let state = if matches!(this.lock().conn.transaction_state(Some(DatabaseName::Main))?, Tx::Read | Tx::Write) {
            State::Txn
        } else {
            State::Init
        };

        Ok((builder, state))
    }

    fn execute_step(
        &mut self,
        step: &Step,
        results: &[bool],
        builder: &mut impl QueryResultBuilder,
    ) -> Result<bool> {
        builder.begin_step()?;

        let mut enabled = match step.cond.as_ref() {
            Some(cond) => match eval_cond(cond, results, self.is_autocommit()) {
                Ok(enabled) => enabled,
                Err(e) => {
                    builder.step_error(e).unwrap();
                    false
                }
            },
            None => true,
        };

        let (affected_row_count, last_insert_rowid) = if enabled {
            match self.execute_query(&step.query, builder) {
                // builder error interupt the execution of query. we should exit immediately.
                Err(e @ Error::BuilderError(_)) => return Err(e),
                Err(e) => {
                    builder.step_error(e)?;
                    enabled = false;
                    (0, None)
                }
                Ok(x) => x,
            }
        } else {
            (0, None)
        };

        builder.finish_step(affected_row_count, last_insert_rowid)?;

        Ok(enabled)
    }

    fn execute_query(
        &self,
        query: &Query,
        builder: &mut impl QueryResultBuilder,
    ) -> Result<(u64, Option<i64>)> {
        tracing::trace!("executing query: {}", query.stmt.stmt);

        let config = self.config_store.get();
        let blocked = match query.stmt.kind {
            StmtKind::Read | StmtKind::TxnBegin | StmtKind::Other => config.block_reads,
            StmtKind::Write => config.block_reads || config.block_writes,
            StmtKind::TxnEnd => false,
        };
        if blocked {
            return Err(Error::Blocked(config.block_reason.clone()));
        }

        let mut stmt = self.conn.prepare(&query.stmt.stmt)?;

        let cols = stmt.columns();
        let cols_count = cols.len();
        builder.cols_description(cols.iter())?;
        drop(cols);

        query
            .params
            .bind(&mut stmt)
            .map_err(Error::LibSqlInvalidQueryParams)?;

        let mut qresult = stmt.raw_query();
        builder.begin_rows()?;
        while let Some(row) = qresult.next()? {
            builder.begin_row()?;
            for i in 0..cols_count {
                let val = row.get_ref(i)?;
                builder.add_row_value(val)?;
            }
            builder.finish_row()?;
        }

        builder.finish_rows()?;

        // sqlite3_changes() is only modified for INSERT, UPDATE or DELETE; it is not reset for SELECT,
        // but we want to return 0 in that case.
        let affected_row_count = match query.stmt.is_iud {
            true => self.conn.changes(),
            false => 0,
        };

        // sqlite3_last_insert_rowid() only makes sense for INSERTs into a rowid table. we can't detect
        // a rowid table, but at least we can detect an INSERT
        let last_insert_rowid = match query.stmt.is_insert {
            true => Some(self.conn.last_insert_rowid()),
            false => None,
        };

        drop(qresult);

        self.update_stats(&stmt);

        Ok((affected_row_count, last_insert_rowid))
    }

    fn rollback(&self) {
        if let Err(e) = self.conn.execute("ROLLBACK", ()) {
            tracing::error!("failed to rollback: {e}");
        }
    }

    fn checkpoint(&self) -> Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", (), |_| Ok(()))?;
        Ok(())
    }

    fn update_stats(&self, stmt: &rusqlite::Statement) {
        let rows_read = stmt.get_status(StatementStatus::RowsRead);
        let rows_written = stmt.get_status(StatementStatus::RowsWritten);
        let rows_read = if rows_read == 0 && rows_written == 0 {
            1
        } else {
            rows_read
        };
        self.stats.inc_rows_read(rows_read as u64);
        self.stats.inc_rows_written(rows_written as u64);
    }

    fn describe(&self, sql: &str) -> DescribeResult {
        let stmt = self.conn.prepare(sql)?;

        let params = (1..=stmt.parameter_count())
            .map(|param_i| {
                let name = stmt.parameter_name(param_i).map(|n| n.into());
                DescribeParam { name }
            })
            .collect();

        let cols = stmt
            .columns()
            .into_iter()
            .map(|col| {
                let name = col.name().into();
                let decltype = col.decl_type().map(|t| t.into());
                DescribeCol { name, decltype }
            })
            .collect();

        let is_explain = stmt.is_explain() != 0;
        let is_readonly = stmt.readonly();
        Ok(DescribeResponse {
            params,
            cols,
            is_explain,
            is_readonly,
        })
    }

    fn is_autocommit(&self) -> bool {
        self.conn.is_autocommit()
    }
}

fn eval_cond(cond: &Cond, results: &[bool], is_autocommit: bool) -> Result<bool> {
    let get_step_res = |step: usize| -> Result<bool> {
        let res = results.get(step).ok_or(Error::InvalidBatchStep(step))?;
        Ok(*res)
    };

    Ok(match cond {
        Cond::Ok { step } => get_step_res(*step)?,
        Cond::Err { step } => !get_step_res(*step)?,
        Cond::Not { cond } => !eval_cond(cond, results, is_autocommit)?,
        Cond::And { conds } => conds.iter().try_fold(true, |x, cond| {
            eval_cond(cond, results, is_autocommit).map(|y| x & y)
        })?,
        Cond::Or { conds } => conds.iter().try_fold(false, |x, cond| {
            eval_cond(cond, results, is_autocommit).map(|y| x | y)
        })?,
        Cond::IsAutocommit => is_autocommit,
    })
}

fn check_program_auth(auth: Authenticated, pgm: &Program) -> Result<()> {
    for step in pgm.steps() {
        let query = &step.query;
        match (query.stmt.kind, &auth) {
            (_, Authenticated::Anonymous) => {
                return Err(Error::NotAuthorized(
                    "anonymous access not allowed".to_string(),
                ));
            }
            (StmtKind::Read, Authenticated::Authorized(_)) => (),
            (StmtKind::TxnBegin, _) | (StmtKind::TxnEnd, _) => (),
            (
                _,
                Authenticated::Authorized(Authorized {
                    permission: Permission::FullAccess,
                    ..
                }),
            ) => (),
            _ => {
                return Err(Error::NotAuthorized(format!(
                    "Current session is not authorized to run: {}",
                    query.stmt.stmt
                )));
            }
        }
    }
    Ok(())
}

fn check_describe_auth(auth: Authenticated) -> Result<()> {
    match auth {
        Authenticated::Anonymous => {
            Err(Error::NotAuthorized("anonymous access not allowed".into()))
        }
        Authenticated::Authorized(_) => Ok(()),
    }
}

#[async_trait::async_trait]
impl<W> super::Connection for LibSqlConnection<W>
where
    W: WalHook + 'static,
    W::Context: Send,
{
    async fn execute_program<B: QueryResultBuilder>(
        &self,
        pgm: Program,
        auth: Authenticated,
        builder: B,
        _replication_index: Option<FrameNo>,
    ) -> Result<(B, State)> {
        check_program_auth(auth, &pgm)?;
        let conn = self.inner.clone();
        tokio::task::spawn_blocking(move || Connection::run(conn, pgm, builder))
            .await
            .unwrap()
    }

    async fn describe(
        &self,
        sql: String,
        auth: Authenticated,
        _replication_index: Option<FrameNo>,
    ) -> Result<DescribeResult> {
        check_describe_auth(auth)?;
        let conn = self.inner.clone();
        let res = tokio::task::spawn_blocking(move || conn.lock().describe(&sql))
            .await
            .unwrap();

        Ok(res)
    }

    async fn is_autocommit(&self) -> Result<bool> {
        Ok(self.inner.lock().is_autocommit())
    }

    async fn checkpoint(&self) -> Result<()> {
        let conn = self.inner.clone();
        tokio::task::spawn_blocking(move || conn.lock().checkpoint())
            .await
            .unwrap()?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use insta::assert_json_snapshot;
    use itertools::Itertools;
    use sqld_libsql_bindings::wal_hook::TRANSPARENT_METHODS;
    use tempfile::tempdir;
    use tokio::task::JoinSet;

    use crate::connection::Connection as _;
    use crate::query_result_builder::test::{test_driver, TestBuilder};
    use crate::query_result_builder::QueryResultBuilder;
    use crate::DEFAULT_AUTO_CHECKPOINT;

    use super::*;

    fn setup_test_conn() -> Arc<Mutex<Connection>> {
        let conn = Connection {
            conn: sqld_libsql_bindings::Connection::test(),
            stats: Arc::new(Stats::default()),
            config_store: Arc::new(DatabaseConfigStore::new_test()),
            builder_config: QueryBuilderConfig::default(),
            current_frame_no_receiver: watch::channel(None).1,
            state: Default::default(),
            slot: None,
        };

        let conn = Arc::new(Mutex::new(conn));

        let stmts = std::iter::once("create table test (x)")
            .chain(std::iter::repeat("insert into test values ('hello world')").take(100))
            .collect_vec();
        Connection::run(conn.clone(), Program::seq(&stmts), TestBuilder::default()).unwrap();

        conn
    }

    #[test]
    fn test_libsql_conn_builder_driver() {
        test_driver(1000, |b| {
            let conn = setup_test_conn();
            Connection::run(conn, Program::seq(&["select * from test"]), b).map(|x| x.0)
        })
    }

    #[tokio::test]
    async fn txn_stealing() {
        let tmp = tempdir().unwrap();
        let make_conn = MakeLibSqlConn::new(
            tmp.path().into(),
            &TRANSPARENT_METHODS,
            || (),
            Default::default(),
            Arc::new(DatabaseConfigStore::load(tmp.path()).unwrap()),
            Arc::new([]),
            100000000,
            100000000,
            DEFAULT_AUTO_CHECKPOINT,
            watch::channel(None).1,
        )
        .await
        .unwrap();

        let conn1 = make_conn.make_connection().await.unwrap();
        let conn2 = make_conn.make_connection().await.unwrap();

        let mut join_set = JoinSet::new();
        let notify = Arc::new(Notify::new());

        join_set.spawn({
            let notify = notify.clone();
            async move {
                // 1. take an exclusive lock
                let conn = conn1.inner.clone();
                let res = tokio::task::spawn_blocking(|| {
                    Connection::run(
                        conn,
                        Program::seq(&["BEGIN EXCLUSIVE"]),
                        TestBuilder::default(),
                    )
                    .unwrap()
                })
                .await
                .unwrap();
                assert!(res.0.into_ret().into_iter().all(|x| x.is_ok()));
                assert_eq!(res.1, State::Txn);
                assert!(conn1.inner.lock().slot.is_some());
                // 2. notify other conn that lock was acquired
                notify.notify_one();
                // 6. wait till other connection steals the lock
                notify.notified().await;
                // 7. get an error because txn timedout
                let conn = conn1.inner.clone();
                // our lock was stolen
                assert!(conn1
                    .inner
                    .lock()
                    .slot
                    .as_ref()
                    .unwrap()
                    .is_stolen
                    .load(Ordering::Relaxed));
                let res = tokio::task::spawn_blocking(|| {
                    Connection::run(
                        conn,
                        Program::seq(&["CREATE TABLE TEST (x)"]),
                        TestBuilder::default(),
                    )
                    .unwrap()
                })
                .await
                .unwrap();

                assert!(matches!(res.0.into_ret()[0], Err(Error::LibSqlTxTimeout)));

                let before = Instant::now();
                let conn = conn1.inner.clone();
                // 8. try to acquire lock again
                let res = tokio::task::spawn_blocking(|| {
                    Connection::run(
                        conn,
                        Program::seq(&["CREATE TABLE TEST (x)"]),
                        TestBuilder::default(),
                    )
                    .unwrap()
                })
                .await
                .unwrap();

                assert!(res.0.into_ret().into_iter().all(|x| x.is_ok()));
                // the lock must have been released before the timeout
                assert!(before.elapsed() < TXN_TIMEOUT);
                notify.notify_one();
            }
        });

        join_set.spawn({
            let notify = notify.clone();
            async move {
                // 3. wait for other connection to acquire lock
                notify.notified().await;
                // 4. try to acquire lock as well
                let conn = conn2.inner.clone();
                tokio::task::spawn_blocking(|| {
                    Connection::run(
                        conn,
                        Program::seq(&["BEGIN EXCLUSIVE"]),
                        TestBuilder::default(),
                    )
                    .unwrap();
                })
                .await
                .unwrap();
                // 5. notify other that we could acquire the lock
                notify.notify_one();

                // 9. rollback before timeout
                tokio::time::sleep(TXN_TIMEOUT / 2).await;
                let conn = conn2.inner.clone();
                let slot = conn2.inner.lock().slot.as_ref().unwrap().clone();
                tokio::task::spawn_blocking(|| {
                    Connection::run(conn, Program::seq(&["ROLLBACK"]), TestBuilder::default())
                        .unwrap();
                })
                .await
                .unwrap();
                // rolling back caused to slot to b removed
                assert!(conn2.inner.lock().slot.is_none());
                // the lock was *not* stolen
                notify.notified().await;
                assert!(!slot.is_stolen.load(Ordering::Relaxed));
            }
        });

        while join_set.join_next().await.is_some() {}
    }

    #[tokio::test]
    async fn txn_timeout_no_stealing() {
        let tmp = tempdir().unwrap();
        let make_conn = MakeLibSqlConn::new(
            tmp.path().into(),
            &TRANSPARENT_METHODS,
            || (),
            Default::default(),
            Arc::new(DatabaseConfigStore::load(tmp.path()).unwrap()),
            Arc::new([]),
            100000000,
            100000000,
            DEFAULT_AUTO_CHECKPOINT,
            watch::channel(None).1,
        )
            .await
            .unwrap();

        tokio::time::pause();
        let conn = make_conn.make_connection().await.unwrap();
        let (_builder, state) = Connection::run(conn.inner.clone(), Program::seq(&["BEGIN IMMEDIATE"]), TestBuilder::default()).unwrap();
        assert_eq!(state, State::Txn);

        tokio::time::advance(TXN_TIMEOUT * 2).await;

        let (builder, state) = Connection::run(conn.inner.clone(), Program::seq(&["BEGIN IMMEDIATE"]), TestBuilder::default()).unwrap();
        assert_eq!(state, State::Init);
        assert!(matches!(builder.into_ret()[0], Err(Error::LibSqlTxTimeout)));
    }
}
