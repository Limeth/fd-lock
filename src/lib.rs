//! Advisory reader-writer locks for files.
//!
//! # Notes on Advisory Locks
//!
//! "advisory locks" are locks which programs must opt-in to adhere to. This
//! means that they can be used to coordinate file access, but not prevent
//! access. Use this to coordinate file access between multiple instances of the
//! same program. But do not use this to prevent actors from accessing or
//! modifying files.
//!
//! # Example
//!
//! ```
//! use std::path::PathBuf;
//! use tokio::fs::File;
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//! use async_fd_lock::{LockRead, LockWrite};
//!
//! # tokio_test::block_on(async {
//! let dir = tempfile::tempdir().unwrap();
//! let path = dir.path().join("foo.txt");
//!
//! // Lock it for writing.
//! {
//!     let mut write_guard = File::create_new(&path).await?.lock_write().await?;
//!     write_guard.write(b"bongo cat").await?;
//! }
//!
//! // Lock it for reading.
//! {
//!     let mut read_guard_1 = File::open(&path).await?.lock_read().await?;
//!     let mut read_guard_2 = File::open(&path).await?.lock_read().await?;
//!     let byte_1 = read_guard_1.read_u8().await?;
//!     let byte_2 = read_guard_2.read_u8().await?;
//! }
//! # std::io::Result::Ok(())
//! # }).unwrap();
//! ```
#![forbid(future_incompatible)]
#![deny(missing_debug_implementations, nonstandard_style)]
#![cfg_attr(doc, warn(missing_docs))]

use sys::AsOpenFileExt;

mod read_guard;
mod write_guard;

pub(crate) mod error;
pub(crate) mod sys;

pub use error::*;
#[cfg(feature = "async")]
pub use nonblocking::*;
pub use read_guard::RwLockReadGuard;
pub use sys::AsOpenFile;
pub use write_guard::RwLockWriteGuard;

pub mod blocking {
    use super::*;

    pub trait LockRead: AsOpenFile + std::io::Read {
        fn lock_read(self) -> LockReadResult<Self>
        where
            Self: Sized;

        fn try_lock_read(self) -> LockReadResult<Self>
        where
            Self: Sized;
    }

    pub trait LockWrite: AsOpenFile + std::io::Write {
        fn lock_write(self) -> LockWriteResult<Self>
        where
            Self: Sized;

        fn try_lock_write(self) -> LockWriteResult<Self>
        where
            Self: Sized;
    }

    impl<T> LockRead for T
    where
        T: AsOpenFile + std::io::Read,
    {
        fn lock_read(self) -> LockReadResult<Self> {
            match self.acquire_lock_blocking::<false, true>() {
                Ok(guard) => Ok(RwLockReadGuard::new(self, guard)),
                Err(error) => Err(LockError::new(self, error)),
            }
        }

        fn try_lock_read(self) -> LockReadResult<Self> {
            match self.acquire_lock_blocking::<false, false>() {
                Ok(guard) => Ok(RwLockReadGuard::new(self, guard)),
                Err(error) => Err(LockError::new(self, error)),
            }
        }
    }

    impl<T> LockWrite for T
    where
        T: AsOpenFile + std::io::Write,
    {
        fn lock_write(self) -> LockWriteResult<Self> {
            match self.acquire_lock_blocking::<true, true>() {
                Ok(guard) => Ok(RwLockWriteGuard::new(self, guard)),
                Err(error) => Err(LockError::new(self, error)),
            }
        }

        fn try_lock_write(self) -> LockWriteResult<Self> {
            match self.acquire_lock_blocking::<true, false>() {
                Ok(guard) => Ok(RwLockWriteGuard::new(self, guard)),
                Err(error) => Err(LockError::new(self, error)),
            }
        }
    }
}

#[cfg(feature = "async")]
pub mod nonblocking {
    use super::*;
    use async_trait::async_trait;
    use std::io;
    use sys::{AsOpenFileExt, RwLockGuard};

    async fn lock<const WRITE: bool, const BLOCK: bool, T>(
        file: &T,
    ) -> Result<RwLockGuard<<T as AsOpenFileExt>::OwnedOpenFile>, io::Error>
    where
        T: AsOpenFile + Sync + 'static,
    {
        let handle = file.borrow_open_file().try_clone_to_owned()?;
        let (sync_send, async_recv) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let guard = handle.acquire_lock_blocking::<WRITE, BLOCK>();
            let result = sync_send.send(guard);
            drop(result); // If the guard cannot be sent to the async task, release the lock immediately.
        });
        async_recv
            .await
            .expect("the blocking task is not cancelable")
    }

    #[async_trait]
    pub trait LockRead: AsOpenFile + tokio::io::AsyncRead {
        async fn lock_read(self) -> LockReadResult<Self>
        where
            Self: Sized;

        async fn try_lock_read(self) -> LockReadResult<Self>
        where
            Self: Sized;
    }

    #[async_trait]
    pub trait LockWrite: AsOpenFile + tokio::io::AsyncWrite {
        async fn lock_write(self) -> LockWriteResult<Self>
        where
            Self: Sized;

        async fn try_lock_write(self) -> LockWriteResult<Self>
        where
            Self: Sized;
    }

    #[async_trait]
    impl<T> LockRead for T
    where
        T: AsOpenFile + tokio::io::AsyncRead + Send + Sync + 'static,
    {
        async fn lock_read(self) -> LockReadResult<Self> {
            match lock::<false, true, _>(&self).await {
                Ok(guard) => Ok(RwLockReadGuard::new(self, guard)),
                Err(error) => Err(LockError::new(self, error)),
            }
        }

        async fn try_lock_read(self) -> LockReadResult<Self> {
            match lock::<false, false, _>(&self).await {
                Ok(guard) => Ok(RwLockReadGuard::new(self, guard)),
                Err(error) => Err(LockError::new(self, error)),
            }
        }
    }

    #[async_trait]
    impl<T> LockWrite for T
    where
        T: AsOpenFile + tokio::io::AsyncWrite + Send + Sync + 'static,
    {
        async fn lock_write(self) -> LockWriteResult<Self> {
            match lock::<true, true, _>(&self).await {
                Ok(guard) => Ok(RwLockWriteGuard::new(self, guard)),
                Err(error) => Err(LockError::new(self, error)),
            }
        }

        async fn try_lock_write(self) -> LockWriteResult<Self> {
            match lock::<true, false, _>(&self).await {
                Ok(guard) => Ok(RwLockWriteGuard::new(self, guard)),
                Err(error) => return Err(LockError::new(self, error)),
            }
        }
    }
}
