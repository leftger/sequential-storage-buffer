//! ISR-safe RAM ring with Embassy Signal wakeup.
//!
//! [`SharedRamRing`] wraps [`RamRing`] in a [`CriticalSectionRawMutex`] blocking mutex so it can
//! be enqueued to from an interrupt handler without holding the critical section while the slow
//! flash drain runs in a task.
//!
//! ## Typical wiring
//!
//! ```ignore
//! use sequential_storage_buffer::{OverflowPolicy, shared::SharedRamRing};
//!
//! static RING: SharedRamRing<256> = SharedRamRing::new();
//!
//! // In an interrupt handler (or anywhere, no async needed):
//! RING.enqueue(&sensor_sample, OverflowPolicy::DiscardOldest);
//!
//! // In the drain task:
//! #[embassy_executor::task]
//! async fn drain(mut storage: QueueStorage<Flash, NoCache>) {
//!     let mut scratch = [0u8; 64];
//!     loop {
//!         RING.wait_and_drain_all(&mut storage, &mut scratch, false).await.unwrap();
//!     }
//! }
//! ```

use core::cell::RefCell;

use embassy_sync::{blocking_mutex::Mutex, blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embedded_storage_async::nor_flash::{MultiwriteNorFlash, NorFlash};
use sequential_storage::{Error, cache::CacheImpl, queue::QueueStorage};

use crate::{OverflowPolicy, RamRing};

/// An ISR-safe RAM ring buffer with an Embassy [`Signal`] that wakes a drain task on enqueue.
///
/// Designed to be placed in a `static`:
/// ```ignore
/// static RING: SharedRamRing<256> = SharedRamRing::new();
/// ```
///
/// The [`enqueue`][SharedRamRing::enqueue] method is synchronous and interrupt-safe.
/// Drain and read methods (`drain_one`, `drain_all`, `pop`, `peek`) are `async` and intended
/// for task context; they take a `&mut QueueStorage` so flash access remains exclusive to
/// one task.
pub struct SharedRamRing<const N: usize> {
    ring: Mutex<CriticalSectionRawMutex, RefCell<RamRing<N>>>,
    signal: Signal<CriticalSectionRawMutex, ()>,
}

impl<const N: usize> SharedRamRing<N> {
    /// Create a new `SharedRamRing`. Suitable for `static` initialisation.
    pub const fn new() -> Self {
        Self {
            ring: Mutex::new(RefCell::new(RamRing::new())),
            signal: Signal::new(),
        }
    }

    // ── Producer API (sync, ISR-safe) ─────────────────────────────────────────

    /// Enqueue an item. Safe to call from any context, including interrupt handlers.
    ///
    /// Signals the drain task after a successful enqueue so it wakes without polling.
    /// Returns `Err(())` if the ring is full and `policy` is [`OverflowPolicy::Err`].
    pub fn enqueue(&self, data: &[u8], policy: OverflowPolicy) -> Result<(), ()> {
        let result = self.ring.lock(|r| match policy {
            OverflowPolicy::Err => r.borrow_mut().push(data),
            OverflowPolicy::DiscardOldest => r.borrow_mut().push_overwriting(data),
        });
        if result.is_ok() {
            self.signal.signal(());
        }
        result
    }

    // ── Task-context API (async) ──────────────────────────────────────────────

    /// Wait until at least one item has been enqueued since the last `wait`.
    ///
    /// This is the low-power wait used in the drain task body:
    /// ```ignore
    /// loop { ring.wait_and_drain_all(&mut storage, &mut scratch, false).await?; }
    /// ```
    pub async fn wait(&self) {
        self.signal.wait().await;
    }

    /// Drain one item from the ring to flash.
    ///
    /// `scratch` must be at least [`oldest_ram_item_len`][Self::oldest_ram_item_len] bytes.
    /// Returns `Ok(true)` if an item was written, `Ok(false)` if the ring was empty.
    ///
    /// The critical section is held only for the brief ring peek/discard; the slow flash
    /// write runs outside it.
    pub async fn drain_one<S: NorFlash, C: CacheImpl>(
        &self,
        storage: &mut QueueStorage<S, C>,
        scratch: &mut [u8],
        allow_overwrite: bool,
    ) -> Result<bool, Error<S::Error>> {
        // Copy the oldest item out of the ring while briefly holding the critical section.
        let len = self.ring.lock(|r| r.borrow().peek_into(scratch).map(|s| s.len()));
        let Some(len) = len else {
            return Ok(false);
        };
        // Flash write runs outside the critical section.
        storage.push(&scratch[..len], allow_overwrite).await?;
        // Discard only after a successful flash write so the item is never silently lost.
        self.ring.lock(|r| r.borrow_mut().discard_oldest());
        Ok(true)
    }

    /// Drain all ring items to flash.
    ///
    /// `scratch` must be large enough for the largest pending item.
    pub async fn drain_all<S: NorFlash, C: CacheImpl>(
        &self,
        storage: &mut QueueStorage<S, C>,
        scratch: &mut [u8],
        allow_overwrite: bool,
    ) -> Result<(), Error<S::Error>> {
        while self.drain_one(storage, scratch, allow_overwrite).await? {}
        Ok(())
    }

    /// Wait for the signal, then drain all ring items to flash.
    ///
    /// This is the recommended drain-task body:
    /// ```ignore
    /// loop {
    ///     ring.wait_and_drain_all(&mut storage, &mut scratch, false).await.unwrap();
    /// }
    /// ```
    pub async fn wait_and_drain_all<S: NorFlash, C: CacheImpl>(
        &self,
        storage: &mut QueueStorage<S, C>,
        scratch: &mut [u8],
        allow_overwrite: bool,
    ) -> Result<(), Error<S::Error>> {
        self.wait().await;
        self.drain_all(storage, scratch, allow_overwrite).await
    }

    /// Pop the oldest item (drains ring to flash first to preserve ordering).
    pub async fn pop<'d, S: MultiwriteNorFlash, C: CacheImpl>(
        &self,
        storage: &mut QueueStorage<S, C>,
        data_buffer: &'d mut [u8],
        allow_overwrite: bool,
    ) -> Result<Option<&'d mut [u8]>, Error<S::Error>> {
        if self.ram_pending_count() > 0 {
            self.drain_all(storage, data_buffer, allow_overwrite).await?;
        }
        storage.pop(data_buffer).await
    }

    /// Peek at the oldest item without removing it (drains ring to flash first).
    pub async fn peek<'d, S: MultiwriteNorFlash, C: CacheImpl>(
        &self,
        storage: &mut QueueStorage<S, C>,
        data_buffer: &'d mut [u8],
        allow_overwrite: bool,
    ) -> Result<Option<&'d mut [u8]>, Error<S::Error>> {
        if self.ram_pending_count() > 0 {
            self.drain_all(storage, data_buffer, allow_overwrite).await?;
        }
        storage.peek(data_buffer).await
    }

    // ── Introspection ─────────────────────────────────────────────────────────

    /// Total capacity of the ring in bytes (including 2-byte per-item length prefixes).
    pub const fn ram_capacity_bytes() -> usize {
        N
    }

    /// Free bytes remaining in the ring.
    pub fn ram_free_bytes(&self) -> usize {
        self.ring.lock(|r| N - r.borrow().bytes_used())
    }

    /// Number of items currently buffered in the ring.
    pub fn ram_pending_count(&self) -> usize {
        self.ring.lock(|r| r.borrow().len())
    }

    /// Byte length of the oldest item in the ring, or `None` if the ring is empty.
    pub fn oldest_ram_item_len(&self) -> Option<usize> {
        self.ring.lock(|r| r.borrow().oldest_len())
    }
}

impl<const N: usize> Default for SharedRamRing<N> {
    fn default() -> Self {
        Self::new()
    }
}
