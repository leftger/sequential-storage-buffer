#![no_std]

//! A RAM-buffered queue on top of [`sequential-storage`][sequential_storage] for NOR flash.
//!
//! NOR flash writes are slow and erases are very slow. If a producer emits data faster than the
//! flash interface can commit it, [`BufferedQueue`] accepts items into a fixed-size RAM ring
//! buffer via the synchronous [`enqueue`][BufferedQueue::enqueue] call (no flash I/O) and
//! asynchronously drains them to flash via [`drain_one`][BufferedQueue::drain_one] or
//! [`drain_all`][BufferedQueue::drain_all].
//!
//! # Ordering
//!
//! FIFO ordering is preserved. [`pop`][BufferedQueue::pop] and [`peek`][BufferedQueue::peek]
//! drain any pending RAM items to flash first, then read from flash.
//!
//! # Power-fail note
//!
//! Items that are in the RAM ring and have not yet been drained to flash **will be lost** on
//! power loss. Items that have been drained follow the power-fail safety guarantees of the
//! underlying `sequential-storage` crate.

mod ram_ring;
pub use ram_ring::RamRing;

use embedded_storage_async::nor_flash::{MultiwriteNorFlash, NorFlash};
use sequential_storage::{
    Error,
    cache::CacheImpl,
    queue::QueueStorage,
};

/// A write-buffered queue that accepts items into a RAM ring and drains them to NOR flash.
///
/// ## Type parameters
///
/// - `S`: NOR flash driver implementing [`NorFlash`].
/// - `C`: Cache implementation from `sequential_storage::cache`.
/// - `RAM_BYTES`: Capacity of the RAM ring buffer in bytes (includes 2-byte per-item overhead).
///
/// ## Usage pattern
///
/// ```ignore
/// // Fast path — called from a tight loop or interrupt handler:
/// queue.enqueue(&sample)?;
///
/// // Slow path — called from a lower-priority task or on a timer:
/// queue.drain_all(&mut scratch, false).await?;
///
/// // Read path (drains any remaining RAM items to flash first, then pops):
/// if let Some(data) = queue.pop(&mut buf).await? {
///     // process data
/// }
/// ```
pub struct BufferedQueue<S: NorFlash, C: CacheImpl, const RAM_BYTES: usize> {
    storage: QueueStorage<S, C>,
    ram: RamRing<RAM_BYTES>,
}

impl<S: NorFlash, C: CacheImpl, const RAM_BYTES: usize> BufferedQueue<S, C, RAM_BYTES> {
    /// Wrap an existing [`QueueStorage`] with a RAM ring buffer.
    pub fn new(storage: QueueStorage<S, C>) -> Self {
        Self {
            storage,
            ram: RamRing::new(),
        }
    }

    /// Enqueue an item into the RAM ring buffer.
    ///
    /// This is **synchronous and never touches flash**. Returns `Err(())` if the ring is full;
    /// the caller can call [`drain_one`][Self::drain_one] to make room and retry, or treat the
    /// failure as a dropped sample.
    ///
    /// To check available space before pushing, use [`oldest_ram_item_len`][Self::oldest_ram_item_len]
    /// or compare [`ram_bytes_used`][Self::ram_bytes_used] against `RAM_BYTES`.
    pub fn enqueue(&mut self, data: &[u8]) -> Result<(), ()> {
        self.ram.push(data)
    }

    /// Drain one item from the RAM ring to flash.
    ///
    /// `scratch` is caller-provided temporary storage; it must be at least as large as the
    /// oldest pending item (check [`oldest_ram_item_len`][Self::oldest_ram_item_len]).
    ///
    /// Returns `Ok(true)` if an item was committed to flash, `Ok(false)` if the ring was empty.
    pub async fn drain_one(
        &mut self,
        scratch: &mut [u8],
        allow_overwrite: bool,
    ) -> Result<bool, Error<S::Error>> {
        let Some(data) = self.ram.peek_into(scratch) else {
            return Ok(false);
        };
        let len = data.len();
        self.storage.push(&scratch[..len], allow_overwrite).await?;
        self.ram.discard_oldest();
        Ok(true)
    }

    /// Drain all RAM-buffered items to flash.
    ///
    /// `scratch` must be large enough for the largest pending item.
    pub async fn drain_all(
        &mut self,
        scratch: &mut [u8],
        allow_overwrite: bool,
    ) -> Result<(), Error<S::Error>> {
        while self.drain_one(scratch, allow_overwrite).await? {}
        Ok(())
    }

    /// Pop the oldest item from the queue.
    ///
    /// Any pending RAM items are drained to flash first to preserve FIFO ordering.
    /// `data_buffer` is used as both the drain scratch space and the pop output buffer;
    /// size it for your largest expected item.
    ///
    /// **Note:** if flash is full and `allow_overwrite` is `false`, the drain step will
    /// return [`Error::FullStorage`]. In that case, call [`storage`][Self::storage]`().pop()`
    /// directly to consume flash items and free space, then retry.
    pub async fn pop<'d>(
        &mut self,
        data_buffer: &'d mut [u8],
        allow_overwrite: bool,
    ) -> Result<Option<&'d mut [u8]>, Error<S::Error>>
    where
        S: MultiwriteNorFlash,
    {
        if !self.ram.is_empty() {
            self.drain_all(data_buffer, allow_overwrite).await?;
        }
        self.storage.pop(data_buffer).await
    }

    /// Peek at the oldest item without removing it.
    ///
    /// Any pending RAM items are drained to flash first to preserve FIFO ordering.
    pub async fn peek<'d>(
        &mut self,
        data_buffer: &'d mut [u8],
        allow_overwrite: bool,
    ) -> Result<Option<&'d mut [u8]>, Error<S::Error>>
    where
        S: MultiwriteNorFlash,
    {
        if !self.ram.is_empty() {
            self.drain_all(data_buffer, allow_overwrite).await?;
        }
        self.storage.peek(data_buffer).await
    }

    /// Byte length of the oldest item in the RAM ring, or `None` if the ring is empty.
    ///
    /// Use this to size the `scratch` buffer passed to [`drain_one`][Self::drain_one].
    pub fn oldest_ram_item_len(&self) -> Option<usize> {
        self.ram.oldest_len()
    }

    /// Number of items currently buffered in RAM (not yet committed to flash).
    pub fn ram_pending_count(&self) -> usize {
        self.ram.len()
    }

    /// Bytes currently occupied in the RAM ring (including 2-byte per-item length prefixes).
    pub fn ram_bytes_used(&self) -> usize {
        self.ram.bytes_used()
    }

    /// Mutable reference to the underlying [`QueueStorage`] for direct access.
    pub fn storage(&mut self) -> &mut QueueStorage<S, C> {
        &mut self.storage
    }

    /// Consume this [`BufferedQueue`] and return the inner [`QueueStorage`].
    ///
    /// **Any items still in the RAM ring are discarded.**
    pub fn into_storage(self) -> QueueStorage<S, C> {
        self.storage
    }
}
