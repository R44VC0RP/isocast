//! Locks for state a panic can leave half-updated.
//!
//! Locks in the engine and daemon are `parking_lot`'s, which don't poison:
//! a panic contained on one thread no longer makes every other thread that
//! touches the same lock panic too, so housekeeping that fails can't end a
//! recording whose data is intact. The few locks whose state must not be
//! used once a mutation was cut short (the engine's and daemon's control
//! state, the program's routes, the scene thread's input and output) are a
//! [`Critical`] instead. Its [`Critical::lock`] is a transaction: if a panic
//! unwinds while the guard is held, the state is marked broken, and the
//! owner finalizes recordings through its emergency path. A [`Critical::read`]
//! guard can't change the state, so a panic under it breaks nothing.
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex, MutexGuard, WaitTimeoutResult};

/// A mutex whose state counts as broken when a panic unwinds through a
/// mutable guard (see the module documentation).
pub struct Critical<T: ?Sized> {
    broken: AtomicBool,
    lock: Mutex<T>,
}

impl<T> Critical<T> {
    pub const fn new(value: T) -> Self {
        Self {
            broken: AtomicBool::new(false),
            lock: Mutex::new(value),
        }
    }
}

impl<T: Default> Default for Critical<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: ?Sized> Critical<T> {
    /// Lock to change the state: a panic before the guard is dropped marks
    /// the state broken.
    pub fn lock(&self) -> Transaction<'_, T> {
        Transaction {
            panicking: std::thread::panicking(),
            guard: self.lock.lock(),
            broken: &self.broken,
        }
    }

    /// [`Self::lock`] without waiting: `None` while another thread holds it.
    pub fn try_lock(&self) -> Option<Transaction<'_, T>> {
        Some(Transaction {
            panicking: std::thread::panicking(),
            guard: self.lock.try_lock()?,
            broken: &self.broken,
        })
    }

    /// Lock only to look: a panic under this guard leaves the state intact.
    pub fn read(&self) -> Read<'_, T> {
        Read(self.lock.lock())
    }

    /// Whether a panic cut a transaction short, so the state may be
    /// inconsistent and recordings must be finalized by the emergency path.
    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::SeqCst)
    }
}

/// Mutable access to a [`Critical`]'s state.
pub struct Transaction<'a, T: ?Sized> {
    guard: MutexGuard<'a, T>,
    broken: &'a AtomicBool,
    /// Already unwinding when locked (a destructor during a panic): like
    /// std's poisoning, only a panic that starts under the guard counts.
    panicking: bool,
}

impl<T: ?Sized> Transaction<'_, T> {
    /// Wait on `condvar`, unlocked meanwhile.
    pub fn wait(&mut self, condvar: &Condvar) {
        condvar.wait(&mut self.guard);
    }

    /// [`Self::wait`] for at most `timeout`.
    pub fn wait_for(&mut self, condvar: &Condvar, timeout: Duration) -> WaitTimeoutResult {
        condvar.wait_for(&mut self.guard, timeout)
    }

    /// Wait on `condvar` while `condition` holds, for at most `timeout`.
    pub fn wait_while_for(
        &mut self,
        condvar: &Condvar,
        timeout: Duration,
        condition: impl FnMut(&mut T) -> bool,
    ) -> WaitTimeoutResult
    where
        T: Sized,
    {
        condvar.wait_while_for(&mut self.guard, condition, timeout)
    }
}

impl<T: ?Sized> Deref for Transaction<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: ?Sized> DerefMut for Transaction<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T: ?Sized> Drop for Transaction<'_, T> {
    fn drop(&mut self) {
        if !self.panicking && std::thread::panicking() {
            self.broken.store(true, Ordering::SeqCst);
        }
    }
}

/// Shared access to a [`Critical`]'s state.
pub struct Read<'a, T: ?Sized>(MutexGuard<'a, T>);

impl<T: ?Sized> Deref for Read<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}
