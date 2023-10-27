// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::env::current_dir;
use std::future::Future;
use std::io::ErrorKind;
use std::marker::PhantomData;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::rc::Weak;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;

use async_trait::async_trait;
use deno_core::error::get_custom_error_class;
use deno_core::error::type_error;
use deno_core::error::AnyError;
use deno_core::futures;
use deno_core::futures::FutureExt;
use deno_core::unsync::spawn;
use deno_core::unsync::spawn_blocking;
use deno_core::AsyncRefCell;
use deno_core::OpState;
use deno_node::PathClean;
use rand::Rng;
use rusqlite::params;
use rusqlite::OpenFlags;
use rusqlite::OptionalExtension;
use rusqlite::Transaction;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::OnceCell;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::AtomicWrite;
use crate::CommitResult;
use crate::Database;
use crate::DatabaseHandler;
use crate::KvEntry;
use crate::MutationKind;
use crate::QueueMessageHandle;
use crate::ReadRange;
use crate::ReadRangeOutput;
use crate::SnapshotReadOptions;
use crate::Value;

const STATEMENT_INC_AND_GET_DATA_VERSION: &str =
  "update data_version set version = version + 1 where k = 0 returning version";
const STATEMENT_KV_RANGE_SCAN: &str =
  "select k, v, v_encoding, version from kv where k >= ? and k < ? order by k asc limit ?";
const STATEMENT_KV_RANGE_SCAN_REVERSE: &str =
  "select k, v, v_encoding, version from kv where k >= ? and k < ? order by k desc limit ?";
const STATEMENT_KV_POINT_GET_VALUE_ONLY: &str =
  "select v, v_encoding from kv where k = ?";
const STATEMENT_KV_POINT_GET_VERSION_ONLY: &str =
  "select version from kv where k = ?";
const STATEMENT_KV_POINT_SET: &str =
  "insert into kv (k, v, v_encoding, version, expiration_ms) values (:k, :v, :v_encoding, :version, :expiration_ms) on conflict(k) do update set v = :v, v_encoding = :v_encoding, version = :version, expiration_ms = :expiration_ms";
const STATEMENT_KV_POINT_DELETE: &str = "delete from kv where k = ?";

const STATEMENT_QUEUE_ADD_READY: &str = "insert into queue (ts, id, data, backoff_schedule, keys_if_undelivered) values(?, ?, ?, ?, ?)";
const STATEMENT_QUEUE_GET_NEXT_READY: &str = "select ts, id, data, backoff_schedule, keys_if_undelivered from queue where ts <= ? order by ts limit 100";
const STATEMENT_QUEUE_GET_EARLIEST_READY: &str =
  "select ts from queue order by ts limit 1";
const STATEMENT_QUEUE_REMOVE_READY: &str = "delete from queue where id = ?";
const STATEMENT_QUEUE_ADD_RUNNING: &str = "insert into queue_running (deadline, id, data, backoff_schedule, keys_if_undelivered) values(?, ?, ?, ?, ?)";
const STATEMENT_QUEUE_REMOVE_RUNNING: &str =
  "delete from queue_running where id = ?";
const STATEMENT_QUEUE_GET_RUNNING_BY_ID: &str = "select deadline, id, data, backoff_schedule, keys_if_undelivered from queue_running where id = ?";
const STATEMENT_QUEUE_GET_RUNNING: &str =
  "select id from queue_running order by deadline limit 100";

const STATEMENT_CREATE_MIGRATION_TABLE: &str = "
create table if not exists migration_state(
  k integer not null primary key,
  version integer not null
)
";

const MIGRATIONS: [&str; 3] = [
  "
create table data_version (
  k integer primary key,
  version integer not null
);
insert into data_version (k, version) values (0, 0);
create table kv (
  k blob primary key,
  v blob not null,
  v_encoding integer not null,
  version integer not null
) without rowid;
",
  "
create table queue (
  ts integer not null,
  id text not null,
  data blob not null,
  backoff_schedule text not null,
  keys_if_undelivered blob not null,

  primary key (ts, id)
);
create table queue_running(
  deadline integer not null,
  id text not null,
  data blob not null,
  backoff_schedule text not null,
  keys_if_undelivered blob not null,

  primary key (deadline, id)
);
",
  "
alter table kv add column seq integer not null default 0;
alter table data_version add column seq integer not null default 0;
alter table kv add column expiration_ms integer not null default -1;
create index kv_expiration_ms_idx on kv (expiration_ms);
",
];

const DISPATCH_CONCURRENCY_LIMIT: usize = 100;
const DEFAULT_BACKOFF_SCHEDULE: [u32; 5] = [100, 1000, 5000, 30000, 60000];

const ERROR_USING_CLOSED_DATABASE: &str = "Attempted to use a closed database";

#[derive(Clone)]
struct ProtectedConn {
  guard: Rc<AsyncRefCell<()>>,
  conn: Arc<Mutex<Option<rusqlite::Connection>>>,
}

#[derive(Clone)]
struct WeakProtectedConn {
  guard: Weak<AsyncRefCell<()>>,
  conn: std::sync::Weak<Mutex<Option<rusqlite::Connection>>>,
}

impl ProtectedConn {
  fn new(conn: rusqlite::Connection) -> Self {
    Self {
      guard: Rc::new(AsyncRefCell::new(())),
      conn: Arc::new(Mutex::new(Some(conn))),
    }
  }

  fn downgrade(&self) -> WeakProtectedConn {
    WeakProtectedConn {
      guard: Rc::downgrade(&self.guard),
      conn: Arc::downgrade(&self.conn),
    }
  }
}

impl WeakProtectedConn {
  fn upgrade(&self) -> Option<ProtectedConn> {
    let guard = self.guard.upgrade()?;
    let conn = self.conn.upgrade()?;
    Some(ProtectedConn { guard, conn })
  }
}

pub struct SqliteDbHandler<P: SqliteDbHandlerPermissions + 'static> {
  pub default_storage_dir: Option<PathBuf>,
  _permissions: PhantomData<P>,
}

pub trait SqliteDbHandlerPermissions {
  fn check_read(&mut self, p: &Path, api_name: &str) -> Result<(), AnyError>;
  fn check_write(&mut self, p: &Path, api_name: &str) -> Result<(), AnyError>;
}

impl<P: SqliteDbHandlerPermissions> SqliteDbHandler<P> {
  pub fn new(default_storage_dir: Option<PathBuf>) -> Self {
    Self {
      default_storage_dir,
      _permissions: PhantomData,
    }
  }
}

#[async_trait(?Send)]
impl<P: SqliteDbHandlerPermissions> DatabaseHandler for SqliteDbHandler<P> {
  type DB = SqliteDb;

  async fn open(
    &self,
    state: Rc<RefCell<OpState>>,
    path: Option<String>,
  ) -> Result<Self::DB, AnyError> {
    // Validate path
    if let Some(path) = &path {
      if path != ":memory:" {
        if path.is_empty() {
          return Err(type_error("Filename cannot be empty"));
        }
        if path.starts_with(':') {
          return Err(type_error(
            "Filename cannot start with ':' unless prefixed with './'",
          ));
        }
        let path = Path::new(path);
        {
          let mut state = state.borrow_mut();
          let permissions = state.borrow_mut::<P>();
          permissions.check_read(path, "Deno.openKv")?;
          permissions.check_write(path, "Deno.openKv")?;
        }
      }
    }

    let (conn, queue_waker_key) = sqlite_retry_loop(|| {
      let path = path.clone();
      let default_storage_dir = self.default_storage_dir.clone();
      async move {
        spawn_blocking(move || {
          let (conn, queue_waker_key) =
            match (path.as_deref(), &default_storage_dir) {
              (Some(":memory:"), _) | (None, None) => {
                (rusqlite::Connection::open_in_memory()?, None)
              }
              (Some(path), _) => {
                let flags =
                  OpenFlags::default().difference(OpenFlags::SQLITE_OPEN_URI);
                let resolved_path = canonicalize_path(&PathBuf::from(path))?;
                (
                  rusqlite::Connection::open_with_flags(path, flags)?,
                  Some(resolved_path),
                )
              }
              (None, Some(path)) => {
                std::fs::create_dir_all(path)?;
                let path = path.join("kv.sqlite3");
                (rusqlite::Connection::open(path.clone())?, Some(path))
              }
            };

          conn.pragma_update(None, "journal_mode", "wal")?;

          Ok::<_, AnyError>((conn, queue_waker_key))
        })
        .await
        .unwrap()
      }
    })
    .await?;
    let conn = ProtectedConn::new(conn);
    SqliteDb::run_tx(conn.clone(), |tx| {
      tx.execute(STATEMENT_CREATE_MIGRATION_TABLE, [])?;

      let current_version: usize = tx
        .query_row(
          "select version from migration_state where k = 0",
          [],
          |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);

      for (i, migration) in MIGRATIONS.iter().enumerate() {
        let version = i + 1;
        if version > current_version {
          tx.execute_batch(migration)?;
          tx.execute(
            "replace into migration_state (k, version) values(?, ?)",
            [&0, &version],
          )?;
        }
      }

      tx.commit()?;

      Ok(())
    })
    .await?;

    let expiration_watcher = spawn(watch_expiration(conn.clone()));

    Ok(SqliteDb {
      conn,
      queue: OnceCell::new(),
      queue_waker_key,
      expiration_watcher,
    })
  }
}

pub struct SqliteDb {
  conn: ProtectedConn,
  queue: OnceCell<SqliteQueue>,
  queue_waker_key: Option<PathBuf>,
  expiration_watcher: deno_core::unsync::JoinHandle<()>,
}

impl Drop for SqliteDb {
  fn drop(&mut self) {
    self.close();
  }
}

async fn sqlite_retry_loop<R, Fut: Future<Output = Result<R, AnyError>>>(
  mut f: impl FnMut() -> Fut,
) -> Result<R, AnyError> {
  loop {
    match f().await {
      Ok(x) => return Ok(x),
      Err(e) => {
        if let Some(x) = e.downcast_ref::<rusqlite::Error>() {
          if x.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) {
            log::debug!("kv: Database is busy, retrying");
            tokio::time::sleep(Duration::from_millis(
              rand::thread_rng().gen_range(5..20),
            ))
            .await;
            continue;
          }
        }
        return Err(e);
      }
    }
  }
}

impl SqliteDb {
  async fn run_tx<F, R>(conn: ProtectedConn, f: F) -> Result<R, AnyError>
  where
    F: (FnOnce(rusqlite::Transaction<'_>) -> Result<R, AnyError>)
      + Clone
      + Send
      + 'static,
    R: Send + 'static,
  {
    sqlite_retry_loop(|| Self::run_tx_inner(conn.clone(), f.clone())).await
  }

  async fn run_tx_inner<F, R>(conn: ProtectedConn, f: F) -> Result<R, AnyError>
  where
    F: (FnOnce(rusqlite::Transaction<'_>) -> Result<R, AnyError>)
      + Send
      + 'static,
    R: Send + 'static,
  {
    // `run_tx` runs in an asynchronous context. First acquire the async lock to
    // coordinate with other async invocations.
    let _guard_holder = conn.guard.borrow_mut().await;

    // Then, take the synchronous lock. This operation is guaranteed to success without waiting,
    // unless the database is being closed.
    let db = conn.conn.clone();
    spawn_blocking(move || {
      let mut db = db.try_lock().ok();
      let Some(db) = db.as_mut().and_then(|x| x.as_mut()) else {
        return Err(type_error(ERROR_USING_CLOSED_DATABASE));
      };
      let result = match db.transaction() {
        Ok(tx) => f(tx),
        Err(e) => Err(e.into()),
      };
      result
    })
    .await
    .unwrap()
  }
}

pub struct DequeuedMessage {
  conn: WeakProtectedConn,
  id: String,
  payload: Option<Vec<u8>>,
  waker_tx: broadcast::Sender<()>,
  _permit: OwnedSemaphorePermit,
}

#[async_trait(?Send)]
impl QueueMessageHandle for DequeuedMessage {
  async fn finish(&self, success: bool) -> Result<(), AnyError> {
    let Some(conn) = self.conn.upgrade() else {
      return Ok(());
    };
    let id = self.id.clone();
    let requeued = SqliteDb::run_tx(conn, move |tx| {
      let requeued = {
        if success {
          let changed = tx
            .prepare_cached(STATEMENT_QUEUE_REMOVE_RUNNING)?
            .execute([&id])?;
          assert!(changed <= 1);
          false
        } else {
          SqliteQueue::requeue_message(&id, &tx)?
        }
      };
      tx.commit()?;
      Ok(requeued)
    })
    .await;
    let requeued = match requeued {
      Ok(x) => x,
      Err(e) => {
        // Silently ignore the error if the database has been closed
        // This message will be delivered on the next run
        if is_conn_closed_error(&e) {
          return Ok(());
        }
        return Err(e);
      }
    };
    if requeued {
      // If the message was requeued, wake up the dequeue loop.
      let _ = self.waker_tx.send(());
    }
    Ok(())
  }

  async fn take_payload(&mut self) -> Result<Vec<u8>, AnyError> {
    self
      .payload
      .take()
      .ok_or_else(|| type_error("Payload already consumed"))
  }
}

type DequeueReceiver = mpsc::Receiver<(Vec<u8>, String)>;

struct SqliteQueue {
  conn: ProtectedConn,
  dequeue_rx: Rc<AsyncRefCell<DequeueReceiver>>,
  concurrency_limiter: Arc<Semaphore>,
  waker_tx: broadcast::Sender<()>,
  shutdown_tx: watch::Sender<()>,
}

impl SqliteQueue {
  fn new(
    conn: ProtectedConn,
    waker_tx: broadcast::Sender<()>,
    waker_rx: broadcast::Receiver<()>,
  ) -> Self {
    let conn_clone = conn.clone();
    let (shutdown_tx, shutdown_rx) = watch::channel::<()>(());
    let (dequeue_tx, dequeue_rx) = mpsc::channel::<(Vec<u8>, String)>(64);

    spawn(async move {
      // Oneshot requeue of all inflight messages.
      if let Err(e) = Self::requeue_inflight_messages(conn.clone()).await {
        // Exit the dequeue loop cleanly if the database has been closed.
        if is_conn_closed_error(&e) {
          return;
        }
        panic!("kv: Error in requeue_inflight_messages: {}", e);
      }

      // Continuous dequeue loop.
      if let Err(e) =
        Self::dequeue_loop(conn.clone(), dequeue_tx, shutdown_rx, waker_rx)
          .await
      {
        // Exit the dequeue loop cleanly if the database has been closed.
        if is_conn_closed_error(&e) {
          return;
        }
        panic!("kv: Error in dequeue_loop: {}", e);
      }
    });

    Self {
      conn: conn_clone,
      dequeue_rx: Rc::new(AsyncRefCell::new(dequeue_rx)),
      waker_tx,
      shutdown_tx,
      concurrency_limiter: Arc::new(Semaphore::new(DISPATCH_CONCURRENCY_LIMIT)),
    }
  }

  async fn dequeue(&self) -> Result<Option<DequeuedMessage>, AnyError> {
    // Wait for the next message to be available from dequeue_rx.
    let (payload, id) = {
      let mut queue_rx = self.dequeue_rx.borrow_mut().await;
      let Some(msg) = queue_rx.recv().await else {
        return Ok(None);
      };
      msg
    };

    let permit = self.concurrency_limiter.clone().acquire_owned().await?;

    Ok(Some(DequeuedMessage {
      conn: self.conn.downgrade(),
      id,
      payload: Some(payload),
      waker_tx: self.waker_tx.clone(),
      _permit: permit,
    }))
  }

  fn shutdown(&self) {
    let _ = self.shutdown_tx.send(());
  }

  async fn dequeue_loop(
    conn: ProtectedConn,
    dequeue_tx: mpsc::Sender<(Vec<u8>, String)>,
    mut shutdown_rx: watch::Receiver<()>,
    mut waker_rx: broadcast::Receiver<()>,
  ) -> Result<(), AnyError> {
    loop {
      let messages = SqliteDb::run_tx(conn.clone(), move |tx| {
        let now = SystemTime::now()
          .duration_since(SystemTime::UNIX_EPOCH)
          .unwrap()
          .as_millis() as u64;

        let messages = tx
          .prepare_cached(STATEMENT_QUEUE_GET_NEXT_READY)?
          .query_map([now], |row| {
            let ts: u64 = row.get(0)?;
            let id: String = row.get(1)?;
            let data: Vec<u8> = row.get(2)?;
            let backoff_schedule: String = row.get(3)?;
            let keys_if_undelivered: String = row.get(4)?;
            Ok((ts, id, data, backoff_schedule, keys_if_undelivered))
          })?
          .collect::<Result<Vec<_>, rusqlite::Error>>()?;

        for (ts, id, data, backoff_schedule, keys_if_undelivered) in &messages {
          let changed = tx
            .prepare_cached(STATEMENT_QUEUE_REMOVE_READY)?
            .execute(params![id])?;
          assert_eq!(changed, 1);

          let changed =
            tx.prepare_cached(STATEMENT_QUEUE_ADD_RUNNING)?.execute(
              params![ts, id, &data, &backoff_schedule, &keys_if_undelivered],
            )?;
          assert_eq!(changed, 1);
        }
        tx.commit()?;

        Ok(
          messages
            .into_iter()
            .map(|(_, id, data, _, _)| (id, data))
            .collect::<Vec<_>>(),
        )
      })
      .await?;

      let busy = !messages.is_empty();

      for (id, data) in messages {
        if dequeue_tx.send((data, id)).await.is_err() {
          // Queue receiver was dropped. Stop the dequeue loop.
          return Ok(());
        }
      }

      if !busy {
        // There's nothing to dequeue right now; sleep until one of the
        // following happens:
        // - It's time to dequeue the next message based on its timestamp
        // - A new message is added to the queue
        // - The database is closed
        let sleep_fut = {
          match Self::get_earliest_ready_ts(conn.clone()).await? {
            Some(ts) => {
              let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
              if ts <= now {
                continue;
              }
              tokio::time::sleep(Duration::from_millis(ts - now)).boxed()
            }
            None => futures::future::pending().boxed(),
          }
        };
        tokio::select! {
          _ = sleep_fut => {}
          x = waker_rx.recv() => {
            if let Err(RecvError::Closed) = x {return Ok(());}
          },
          _ = shutdown_rx.changed() => return Ok(())
        }
      }
    }
  }

  async fn get_earliest_ready_ts(
    conn: ProtectedConn,
  ) -> Result<Option<u64>, AnyError> {
    SqliteDb::run_tx(conn.clone(), move |tx| {
      let ts = tx
        .prepare_cached(STATEMENT_QUEUE_GET_EARLIEST_READY)?
        .query_row([], |row| {
          let ts: u64 = row.get(0)?;
          Ok(ts)
        })
        .optional()?;
      Ok(ts)
    })
    .await
  }

  async fn requeue_inflight_messages(
    conn: ProtectedConn,
  ) -> Result<(), AnyError> {
    loop {
      let done = SqliteDb::run_tx(conn.clone(), move |tx| {
        let entries = tx
          .prepare_cached(STATEMENT_QUEUE_GET_RUNNING)?
          .query_map([], |row| {
            let id: String = row.get(0)?;
            Ok(id)
          })?
          .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        for id in &entries {
          Self::requeue_message(id, &tx)?;
        }
        tx.commit()?;
        Ok(entries.is_empty())
      })
      .await?;
      if done {
        return Ok(());
      }
    }
  }

  fn requeue_message(
    id: &str,
    tx: &rusqlite::Transaction<'_>,
  ) -> Result<bool, AnyError> {
    let Some((_, id, data, backoff_schedule, keys_if_undelivered)) = tx
      .prepare_cached(STATEMENT_QUEUE_GET_RUNNING_BY_ID)?
      .query_row([id], |row| {
        let deadline: u64 = row.get(0)?;
        let id: String = row.get(1)?;
        let data: Vec<u8> = row.get(2)?;
        let backoff_schedule: String = row.get(3)?;
        let keys_if_undelivered: String = row.get(4)?;
        Ok((deadline, id, data, backoff_schedule, keys_if_undelivered))
      })
      .optional()?
    else {
      return Ok(false);
    };

    let backoff_schedule = {
      let backoff_schedule =
        serde_json::from_str::<Option<Vec<u64>>>(&backoff_schedule)?;
      backoff_schedule.unwrap_or_default()
    };

    let mut requeued = false;
    if !backoff_schedule.is_empty() {
      // Requeue based on backoff schedule
      let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
      let new_ts = now + backoff_schedule[0];
      let new_backoff_schedule = serde_json::to_string(&backoff_schedule[1..])?;
      let changed = tx
        .prepare_cached(STATEMENT_QUEUE_ADD_READY)?
        .execute(params![
          new_ts,
          id,
          &data,
          &new_backoff_schedule,
          &keys_if_undelivered
        ])
        .unwrap();
      assert_eq!(changed, 1);
      requeued = true;
    } else if !keys_if_undelivered.is_empty() {
      // No more requeues. Insert the message into the undelivered queue.
      let keys_if_undelivered =
        serde_json::from_str::<Vec<Vec<u8>>>(&keys_if_undelivered)?;

      let version: i64 = tx
        .prepare_cached(STATEMENT_INC_AND_GET_DATA_VERSION)?
        .query_row([], |row| row.get(0))?;

      for key in keys_if_undelivered {
        let changed = tx
          .prepare_cached(STATEMENT_KV_POINT_SET)?
          .execute(params![key, &data, &VALUE_ENCODING_V8, &version, -1i64])?;
        assert_eq!(changed, 1);
      }
    }

    // Remove from running
    let changed = tx
      .prepare_cached(STATEMENT_QUEUE_REMOVE_RUNNING)?
      .execute(params![id])?;
    assert_eq!(changed, 1);

    Ok(requeued)
  }
}

async fn watch_expiration(db: ProtectedConn) {
  loop {
    // Scan for expired keys
    let res = SqliteDb::run_tx(db.clone(), move |tx| {
      let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
      tx.prepare_cached(
        "delete from kv where expiration_ms >= 0 and expiration_ms <= ?",
      )?
      .execute(params![now])?;
      tx.commit()?;
      Ok(())
    })
    .await;
    if let Err(e) = res {
      eprintln!("kv: Error in expiration watcher: {}", e);
    }
    let sleep_duration =
      Duration::from_secs_f64(60.0 + rand::thread_rng().gen_range(0.0..30.0));
    tokio::time::sleep(sleep_duration).await;
  }
}

#[async_trait(?Send)]
impl Database for SqliteDb {
  type QMH = DequeuedMessage;

  async fn snapshot_read(
    &self,
    _state: Rc<RefCell<OpState>>,
    requests: Vec<ReadRange>,
    _options: SnapshotReadOptions,
  ) -> Result<Vec<ReadRangeOutput>, AnyError> {
    let requests = Arc::new(requests);
    Self::run_tx(self.conn.clone(), move |tx| {
      let mut responses = Vec::with_capacity(requests.len());
      for request in &*requests {
        let mut stmt = tx.prepare_cached(if request.reverse {
          STATEMENT_KV_RANGE_SCAN_REVERSE
        } else {
          STATEMENT_KV_RANGE_SCAN
        })?;
        let entries = stmt
          .query_map(
            (
              request.start.as_slice(),
              request.end.as_slice(),
              request.limit.get(),
            ),
            |row| {
              let key: Vec<u8> = row.get(0)?;
              let value: Vec<u8> = row.get(1)?;
              let encoding: i64 = row.get(2)?;

              let value = decode_value(value, encoding);

              let version: i64 = row.get(3)?;
              Ok(KvEntry {
                key,
                value,
                versionstamp: version_to_versionstamp(version),
              })
            },
          )?
          .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        responses.push(ReadRangeOutput { entries });
      }

      Ok(responses)
    })
    .await
  }

  async fn atomic_write(
    &self,
    state: Rc<RefCell<OpState>>,
    write: AtomicWrite,
  ) -> Result<Option<CommitResult>, AnyError> {
    let write = Arc::new(write);
    let (has_enqueues, commit_result) =
      Self::run_tx(self.conn.clone(), move |tx| {
        for check in &write.checks {
          let real_versionstamp = tx
            .prepare_cached(STATEMENT_KV_POINT_GET_VERSION_ONLY)?
            .query_row([check.key.as_slice()], |row| row.get(0))
            .optional()?
            .map(version_to_versionstamp);
          if real_versionstamp != check.versionstamp {
            return Ok((false, None));
          }
        }

        let version: i64 = tx
          .prepare_cached(STATEMENT_INC_AND_GET_DATA_VERSION)?
          .query_row([], |row| row.get(0))?;

        for mutation in &write.mutations {
          match &mutation.kind {
            MutationKind::Set(value) => {
              let (value, encoding) = encode_value(value);
              let changed =
                tx.prepare_cached(STATEMENT_KV_POINT_SET)?.execute(params![
                  mutation.key,
                  value,
                  &encoding,
                  &version,
                  mutation
                    .expire_at
                    .and_then(|x| i64::try_from(x).ok())
                    .unwrap_or(-1i64)
                ])?;
              assert_eq!(changed, 1)
            }
            MutationKind::Delete => {
              let changed = tx
                .prepare_cached(STATEMENT_KV_POINT_DELETE)?
                .execute(params![mutation.key])?;
              assert!(changed == 0 || changed == 1)
            }
            MutationKind::Sum(operand) => {
              mutate_le64(
                &tx,
                &mutation.key,
                "sum",
                operand,
                version,
                |a, b| a.wrapping_add(b),
              )?;
            }
            MutationKind::Min(operand) => {
              mutate_le64(
                &tx,
                &mutation.key,
                "min",
                operand,
                version,
                |a, b| a.min(b),
              )?;
            }
            MutationKind::Max(operand) => {
              mutate_le64(
                &tx,
                &mutation.key,
                "max",
                operand,
                version,
                |a, b| a.max(b),
              )?;
            }
          }
        }

        let now = SystemTime::now()
          .duration_since(SystemTime::UNIX_EPOCH)
          .unwrap()
          .as_millis() as u64;

        let has_enqueues = !write.enqueues.is_empty();
        for enqueue in &write.enqueues {
          let id = Uuid::new_v4().to_string();
          let backoff_schedule = serde_json::to_string(
            &enqueue
              .backoff_schedule
              .as_deref()
              .or_else(|| Some(&DEFAULT_BACKOFF_SCHEDULE[..])),
          )?;
          let keys_if_undelivered =
            serde_json::to_string(&enqueue.keys_if_undelivered)?;

          let changed =
            tx.prepare_cached(STATEMENT_QUEUE_ADD_READY)?
              .execute(params![
                now + enqueue.delay_ms,
                id,
                &enqueue.payload,
                &backoff_schedule,
                &keys_if_undelivered
              ])?;
          assert_eq!(changed, 1)
        }

        tx.commit()?;
        let new_versionstamp = version_to_versionstamp(version);

        Ok((
          has_enqueues,
          Some(CommitResult {
            versionstamp: new_versionstamp,
          }),
        ))
      })
      .await?;

    if has_enqueues {
      match self.queue.get() {
        Some(queue) => {
          let _ = queue.waker_tx.send(());
        }
        None => {
          if let Some(waker_key) = &self.queue_waker_key {
            let (waker_tx, _) =
              shared_queue_waker_channel(waker_key, state.clone());
            let _ = waker_tx.send(());
          }
        }
      }
    }
    Ok(commit_result)
  }

  async fn dequeue_next_message(
    &self,
    state: Rc<RefCell<OpState>>,
  ) -> Result<Option<Self::QMH>, AnyError> {
    let queue = self
      .queue
      .get_or_init(|| async move {
        let (waker_tx, waker_rx) = {
          match &self.queue_waker_key {
            Some(waker_key) => {
              shared_queue_waker_channel(waker_key, state.clone())
            }
            None => broadcast::channel(1),
          }
        };
        SqliteQueue::new(self.conn.clone(), waker_tx, waker_rx)
      })
      .await;
    let handle = queue.dequeue().await?;
    Ok(handle)
  }

  fn close(&self) {
    if let Some(queue) = self.queue.get() {
      queue.shutdown();
    }

    self.expiration_watcher.abort();

    // The above `abort()` operation is asynchronous. It's not
    // guaranteed that the sqlite connection will be closed immediately.
    // So here we synchronously take the conn mutex and drop the connection.
    //
    // This blocks the event loop if the connection is still being used,
    // but ensures correctness - deleting the database file after calling
    // the `close` method will always work.
    self.conn.conn.lock().unwrap().take();
  }
}

/// Mutates a LE64 value in the database, defaulting to setting it to the
/// operand if it doesn't exist.
fn mutate_le64(
  tx: &Transaction,
  key: &[u8],
  op_name: &str,
  operand: &Value,
  new_version: i64,
  mutate: impl FnOnce(u64, u64) -> u64,
) -> Result<(), AnyError> {
  let Value::U64(operand) = *operand else {
    return Err(type_error(format!(
      "Failed to perform '{op_name}' mutation on a non-U64 operand"
    )));
  };

  let old_value = tx
    .prepare_cached(STATEMENT_KV_POINT_GET_VALUE_ONLY)?
    .query_row([key], |row| {
      let value: Vec<u8> = row.get(0)?;
      let encoding: i64 = row.get(1)?;

      let value = decode_value(value, encoding);
      Ok(value)
    })
    .optional()?;

  let new_value = match old_value {
    Some(Value::U64(old_value) ) => mutate(old_value, operand),
    Some(_) => return Err(type_error(format!("Failed to perform '{op_name}' mutation on a non-U64 value in the database"))),
    None => operand,
  };

  let new_value = Value::U64(new_value);
  let (new_value, encoding) = encode_value(&new_value);

  let changed = tx.prepare_cached(STATEMENT_KV_POINT_SET)?.execute(params![
    key,
    &new_value[..],
    encoding,
    new_version,
    -1i64,
  ])?;
  assert_eq!(changed, 1);

  Ok(())
}

fn version_to_versionstamp(version: i64) -> [u8; 10] {
  let mut versionstamp = [0; 10];
  versionstamp[..8].copy_from_slice(&version.to_be_bytes());
  versionstamp
}

const VALUE_ENCODING_V8: i64 = 1;
const VALUE_ENCODING_LE64: i64 = 2;
const VALUE_ENCODING_BYTES: i64 = 3;

fn decode_value(value: Vec<u8>, encoding: i64) -> crate::Value {
  match encoding {
    VALUE_ENCODING_V8 => crate::Value::V8(value),
    VALUE_ENCODING_BYTES => crate::Value::Bytes(value),
    VALUE_ENCODING_LE64 => {
      let mut buf = [0; 8];
      buf.copy_from_slice(&value);
      crate::Value::U64(u64::from_le_bytes(buf))
    }
    _ => todo!(),
  }
}

fn encode_value(value: &crate::Value) -> (Cow<'_, [u8]>, i64) {
  match value {
    crate::Value::V8(value) => (Cow::Borrowed(value), VALUE_ENCODING_V8),
    crate::Value::Bytes(value) => (Cow::Borrowed(value), VALUE_ENCODING_BYTES),
    crate::Value::U64(value) => {
      let mut buf = [0; 8];
      buf.copy_from_slice(&value.to_le_bytes());
      (Cow::Owned(buf.to_vec()), VALUE_ENCODING_LE64)
    }
  }
}

pub struct QueueWaker {
  wakers_tx: HashMap<PathBuf, broadcast::Sender<()>>,
}

fn shared_queue_waker_channel(
  waker_key: &Path,
  state: Rc<RefCell<OpState>>,
) -> (broadcast::Sender<()>, broadcast::Receiver<()>) {
  let mut state = state.borrow_mut();
  let waker = {
    let waker = state.try_borrow_mut::<QueueWaker>();
    match waker {
      Some(waker) => waker,
      None => {
        let waker = QueueWaker {
          wakers_tx: HashMap::new(),
        };
        state.put::<QueueWaker>(waker);
        state.borrow_mut::<QueueWaker>()
      }
    }
  };

  let waker_tx = waker
    .wakers_tx
    .entry(waker_key.to_path_buf())
    .or_insert_with(|| {
      let (waker_tx, _) = broadcast::channel(1);
      waker_tx
    });

  (waker_tx.clone(), waker_tx.subscribe())
}

/// Same as Path::canonicalize, but also handles non-existing paths.
fn canonicalize_path(path: &Path) -> Result<PathBuf, AnyError> {
  let path = path.to_path_buf().clean();
  let mut path = path;
  let mut names_stack = Vec::new();
  loop {
    match path.canonicalize() {
      Ok(mut canonicalized_path) => {
        for name in names_stack.into_iter().rev() {
          canonicalized_path = canonicalized_path.join(name);
        }
        return Ok(canonicalized_path);
      }
      Err(err) if err.kind() == ErrorKind::NotFound => {
        let file_name = path.file_name().map(|os_str| os_str.to_os_string());
        if let Some(file_name) = file_name {
          names_stack.push(file_name.to_str().unwrap().to_string());
          path = path.parent().unwrap().to_path_buf();
        } else {
          names_stack.push(path.to_str().unwrap().to_string());
          let current_dir = current_dir()?;
          path = current_dir.clone();
        }
      }
      Err(err) => return Err(err.into()),
    }
  }
}

fn is_conn_closed_error(e: &AnyError) -> bool {
  get_custom_error_class(e) == Some("TypeError")
    && e.to_string() == ERROR_USING_CLOSED_DATABASE
}
