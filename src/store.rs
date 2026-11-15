//! Persistence for in-flight operations: signatures saved before submission, transfer pointers
//! and results, so an interrupted flow resumes instead of signing twice.

use std::{
    collections::HashMap,
    fmt,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, PoisonError, Weak},
};

use alloy_primitives::{Address, hex, keccak256};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::{Error, Result, error::StoreError, types::EscrowEnvironment};

/// The future a [`Store`] method returns.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

/// A key-value backend for the SDK's records.
///
/// Values are opaque bytes the SDK writes (versioned JSON). One store may serve several
/// environments and signers: the SDK namespaces its keys. Methods added later get default bodies.
///
/// # Contract
///
/// - One writer process per store.
/// - `save` is durable when it returns `Ok`; `delete` of an absent key is `Ok`.
/// - **Operations stay ordered across cancellation.** A future dropped after its operation
///   started (a blocking write handed to another thread, a request in flight) must not let a
///   later `load`, `save` or `delete` overtake it: the later operation waits until the earlier
///   one has completed or been abandoned. Otherwise a stale write from a dropped flow could
///   overwrite the record of its retry.
pub trait Store: Send + Sync + 'static {
    /// The value under `key`, if any.
    fn load<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<Vec<u8>>>;
    /// Stores `value` under `key`, durably on `Ok`.
    fn save<'a>(&'a self, key: &'a str, value: &'a [u8]) -> StoreFuture<'a, ()>;
    /// Removes `key`; `Ok` if it is absent.
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()>;
}

/// The persistence context of sessions: a [`Store`] plus the per-key locks every store-backed
/// flow section takes.
///
/// Cheap to clone; clones share the backend and the locks. Sessions that share persistence must
/// share one journal (clone it), never two journals over one backend, or their locks would not
/// exclude each other.
///
/// Withdrawal outcomes, transfer pointers and markers stay by design, so the same request keeps
/// finding its outcome: size or rotate the journal accordingly.
#[derive(Clone)]
pub struct Journal {
    inner: Arc<JournalInner>,
}

struct JournalInner {
    backend: Box<dyn Store>,
    locks: KeyedLocks,
}

impl Journal {
    /// A journal over `backend`.
    pub fn new(backend: impl Store) -> Self {
        Self {
            inner: Arc::new(JournalInner {
                backend: Box::new(backend),
                locks: KeyedLocks::default(),
            }),
        }
    }
}

impl Journal {
    /// Holds `key`'s lock until the guard drops: one flow section per key at a time across every
    /// clone of this journal.
    pub(crate) async fn lock(&self, key: &str) -> KeyGuard {
        self.inner.locks.lock(key).await
    }

    pub(crate) async fn load(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        self.inner.backend.load(key).await
    }

    pub(crate) async fn save(&self, key: &str, value: &[u8]) -> io::Result<()> {
        self.inner.backend.save(key, value).await
    }

    pub(crate) async fn delete(&self, key: &str) -> io::Result<()> {
        self.inner.backend.delete(key).await
    }
}

impl<S: Store> From<S> for Journal {
    fn from(backend: S) -> Self {
        Self::new(backend)
    }
}

impl fmt::Debug for Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Journal").finish_non_exhaustive()
    }
}

/// The key of `record` for `signer` in `environment`: `"{environment}/{signer}/{record}"`, the
/// address in lowercase hex. One store then serves several environments and keys without one's
/// outcome answering another's request.
pub(crate) fn scoped_key(environment: EscrowEnvironment, signer: Address, record: &str) -> String {
    format!("{environment}/{signer:#x}/{record}")
}

/// A held per-key lock.
pub(crate) type KeyGuard = OwnedMutexGuard<()>;

/// Async mutexes by key, created on demand and dropped with their last user.
#[derive(Default)]
struct KeyedLocks {
    locks: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
}

impl KeyedLocks {
    async fn lock(&self, key: &str) -> KeyGuard {
        let lock = {
            let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
                lock
            } else {
                locks.retain(|_, lock| lock.strong_count() > 0);
                let lock = Arc::new(AsyncMutex::new(()));
                locks.insert(key.to_owned(), Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
}

/// An in-memory store, for tests and processes that need no recovery after a restart.
///
/// Not `Clone`: share it through a [`Journal`].
#[derive(Debug, Default)]
pub struct MemoryStore {
    values: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn values(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Store for MemoryStore {
    fn load<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<Vec<u8>>> {
        let value = self.values().get(key).cloned();
        Box::pin(std::future::ready(Ok(value)))
    }

    fn save<'a>(&'a self, key: &'a str, value: &'a [u8]) -> StoreFuture<'a, ()> {
        self.values().insert(key.to_owned(), value.to_vec());
        Box::pin(std::future::ready(Ok(())))
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.values().remove(key);
        Box::pin(std::future::ready(Ok(())))
    }
}

/// A directory of record files: one file per key, written atomically and durably.
///
/// A file's name is the hex keccak-256 of its key (64 characters, within every file system's name
/// limit); the file holds the value.
/// `open` locks `<dir>/.lock` for the store's life, so a second store over the same directory —
/// in this process or another — fails to open. Operations run on Tokio's blocking pool, in the
/// order they were issued, even when their futures are dropped.
pub struct FileStore {
    inner: Arc<FileInner>,
    /// Orders operations: taken before blocking work starts and released when it ends, also when
    /// the caller's future was dropped in between.
    gate: Arc<AsyncMutex<()>>,
}

struct FileInner {
    dir: PathBuf,
    /// Held for the store's life (the lock is released when the file closes).
    _lock: File,
    #[cfg(test)]
    before_write: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl FileStore {
    /// Opens (creating it if needed) the store in `dir`.
    ///
    /// Fails with [`Error::Store`] if the directory cannot be created or another store holds it.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let lock_path = dir.join(".lock");
        let store_error = |source| {
            Error::Store(Box::new(StoreError::new(lock_path.display().to_string(), false, source)))
        };
        fs::create_dir_all(&dir).map_err(store_error)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(store_error)?;
        lock.try_lock().map_err(|error| store_error(io::Error::from(error)))?;
        Ok(Self {
            inner: Arc::new(FileInner {
                dir,
                _lock: lock,
                #[cfg(test)]
                before_write: Mutex::new(None),
            }),
            gate: Arc::new(AsyncMutex::new(())),
        })
    }

    /// Runs `operation` on the blocking pool after every earlier operation has finished.
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&FileInner) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let turn = Arc::clone(&self.gate).lock_owned().await;
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let _turn = turn;
            operation(&inner)
        })
        .await
        .map_err(io::Error::other)?
    }
}

impl FileInner {
    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(hex::encode(keccak256(key.as_bytes())))
    }

    fn load(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.path(key)) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn save(&self, key: &str, value: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if let Some(hook) = self.before_write.lock().unwrap_or_else(PoisonError::into_inner).take()
        {
            hook();
        }
        let path = self.path(key);
        let temporary = path.with_extension("tmp");
        let mut file = File::create(&temporary)?;
        file.write_all(value)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)?;
        self.sync_dir()
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        match fs::remove_file(self.path(key)) {
            Ok(()) => self.sync_dir(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Makes a rename or removal durable. Directories cannot be opened as files on Windows,
    /// where the rename itself is durable once it returns.
    fn sync_dir(&self) -> io::Result<()> {
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

impl Store for FileStore {
    fn load<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<Vec<u8>>> {
        let key = key.to_owned();
        Box::pin(self.run(move |inner| inner.load(&key)))
    }

    fn save<'a>(&'a self, key: &'a str, value: &'a [u8]) -> StoreFuture<'a, ()> {
        let (key, value) = (key.to_owned(), value.to_vec());
        Box::pin(self.run(move |inner| inner.save(&key, &value)))
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        let key = key.to_owned();
        Box::pin(self.run(move |inner| inner.delete(&key)))
    }
}

impl fmt::Debug for FileStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileStore").field("dir", &self.inner.dir).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::address;

    use super::*;

    const SIGNER: Address = address!("0x2C7536E3605D9C16a7a3D7b1898e529396a65c23");
    const OTHER: Address = address!("0x00000000000000000000000000000000000000aa");

    #[tokio::test]
    async fn memory_store_semantics() {
        let store = MemoryStore::new();
        assert_eq!(store.load("k").await.unwrap(), None);
        store.save("k", b"one").await.unwrap();
        store.save("k", b"two").await.unwrap();
        assert_eq!(store.load("k").await.unwrap().as_deref(), Some(&b"two"[..]));
        store.delete("k").await.unwrap();
        store.delete("k").await.unwrap();
        assert_eq!(store.load("k").await.unwrap(), None);
    }

    #[test]
    fn keys_are_namespaced_by_environment_and_signer() {
        let key = scoped_key(EscrowEnvironment::Production, SIGNER, "withdrawal:w-1");
        assert_eq!(key, "production/0x2c7536e3605d9c16a7a3d7b1898e529396a65c23/withdrawal:w-1");
        let keys = [
            key,
            scoped_key(EscrowEnvironment::Development, SIGNER, "withdrawal:w-1"),
            scoped_key(EscrowEnvironment::Production, OTHER, "withdrawal:w-1"),
        ];
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[0], keys[2]);
    }

    #[tokio::test]
    async fn same_record_under_two_namespaces_gives_two_records() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(FileStore::open(dir.path()).unwrap());
        let production = scoped_key(EscrowEnvironment::Production, SIGNER, "withdrawal:w");
        let development = scoped_key(EscrowEnvironment::Development, SIGNER, "withdrawal:w");
        journal.save(&production, b"p").await.unwrap();
        journal.save(&development, b"d").await.unwrap();
        assert_eq!(journal.load(&production).await.unwrap().as_deref(), Some(&b"p"[..]));
        assert_eq!(journal.load(&development).await.unwrap().as_deref(), Some(&b"d"[..]));
        journal.delete(&production).await.unwrap();
        assert_eq!(journal.load(&production).await.unwrap(), None);
        assert_eq!(journal.load(&development).await.unwrap().as_deref(), Some(&b"d"[..]));
    }

    #[tokio::test]
    async fn journal_clones_share_key_locks() {
        let journal = Journal::new(MemoryStore::new());
        let other = journal.clone();
        let held = journal.lock("withdrawal:w").await;
        let waiting = tokio::spawn(async move {
            let _guard = other.lock("withdrawal:w").await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "a clone must wait for the held key");
        // Another key is independent.
        tokio::time::timeout(Duration::from_secs(1), journal.lock("withdrawal:x")).await.unwrap();
        drop(held);
        tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unused_locks_are_dropped() {
        let locks = KeyedLocks::default();
        drop(locks.lock("a").await);
        drop(locks.lock("b").await);
        let map = locks.locks.lock().unwrap();
        assert!(map.len() <= 1, "dead entries are pruned when a new key is locked");
        assert!(map.values().all(|lock| lock.strong_count() == 0));
    }

    #[tokio::test]
    async fn file_store_round_trip_with_digest_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        store.save("take:1", b"{\"v\":1}").await.unwrap();
        let name = hex::encode(keccak256("take:1"));
        assert!(dir.path().join(&name).is_file());
        assert!(!dir.path().join(format!("{name}.tmp")).exists(), "no temporary file left");
        assert_eq!(store.load("take:1").await.unwrap().as_deref(), Some(&b"{\"v\":1}"[..]));
        store.save("take:1", b"").await.unwrap();
        assert_eq!(store.load("take:1").await.unwrap().as_deref(), Some(&b""[..]));
        store.delete("take:1").await.unwrap();
        assert_eq!(store.load("take:1").await.unwrap(), None);
        store.delete("take:1").await.unwrap();
    }

    #[tokio::test]
    async fn file_store_handles_maximum_length_ids_in_both_environments() {
        let dir = tempfile::tempdir().unwrap();
        let id = "a".repeat(64);
        let keys = [EscrowEnvironment::Production, EscrowEnvironment::Development]
            .map(|environment| scoped_key(environment, SIGNER, &format!("withdrawal:{id}")));
        {
            let store = FileStore::open(dir.path()).unwrap();
            for key in &keys {
                store.save(key, key.as_bytes()).await.unwrap();
            }
        }
        let store = FileStore::open(dir.path()).unwrap();
        for key in &keys {
            assert_eq!(store.load(key).await.unwrap().as_deref(), Some(key.as_bytes()));
            store.delete(key).await.unwrap();
            assert_eq!(store.load(key).await.unwrap(), None);
        }
    }

    #[test]
    fn second_open_fails_while_the_first_lives() {
        let dir = tempfile::tempdir().unwrap();
        let first = FileStore::open(dir.path()).unwrap();
        let error = FileStore::open(dir.path()).unwrap_err();
        let Error::Store(error) = error else { panic!("expected a store error, got {error:?}") };
        assert!(!error.submitted());
        drop(first);
        FileStore::open(dir.path()).unwrap();
    }

    /// A save whose future is dropped while its blocking write is stalled; returns the release.
    async fn stalled_save(
        store: &FileStore,
        key: &str,
        value: &[u8],
    ) -> std::sync::mpsc::Sender<()> {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        *store.inner.before_write.lock().unwrap() = Some(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        let save = store.save(key, value);
        tokio::select! {
            result = save => panic!("the stalled save completed: {result:?}"),
            entered = entered_rx => entered.unwrap(),
        }
        // `save` is dropped here; its blocking write is still running.
        release_tx
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_save_cannot_overwrite_a_later_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileStore::open(dir.path()).unwrap());
        let release = stalled_save(&store, "k", b"stale").await;
        let retry = tokio::spawn({
            let store = Arc::clone(&store);
            async move { store.save("k", b"retry").await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!retry.is_finished(), "the retry waits for the dropped write");
        release.send(()).unwrap();
        retry.await.unwrap().unwrap();
        assert_eq!(store.load("k").await.unwrap().as_deref(), Some(&b"retry"[..]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_after_a_dropped_save_observes_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileStore::open(dir.path()).unwrap());
        let release = stalled_save(&store, "k", b"first").await;
        let load = tokio::spawn({
            let store = Arc::clone(&store);
            async move { store.load("k").await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.send(()).unwrap();
        assert_eq!(load.await.unwrap().unwrap().as_deref(), Some(&b"first"[..]));
    }

    #[test]
    fn shared_types_are_send_sync_and_futures_send() {
        fn check<T: Send + Sync + 'static>() {}
        fn send<T: Send>(_: T) {}
        let journal = Journal::new(MemoryStore::new());
        send(journal.lock("k"));
        send(journal.save("k", b"v"));
        check::<Journal>();
        check::<MemoryStore>();
        check::<FileStore>();
    }
}
