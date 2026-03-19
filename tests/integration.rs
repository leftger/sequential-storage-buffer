// Integration tests using sequential-storage's MockFlash.
//
// Run with:
//   cargo test --features sequential-storage-buffer/test-utils

use futures::executor::block_on;
use sequential_storage::{
    cache::NoCache,
    mock_flash::MockFlashBase,
    queue::{QueueConfig, QueueStorage},
};
use sequential_storage_buffer::BufferedQueue;

// 4 pages × 64 words × 4 bytes/word = 1 KiB flash
type MockFlash = MockFlashBase<4, 4, 64>;

fn make_queue() -> BufferedQueue<MockFlash, NoCache, 256> {
    let flash = MockFlash::new(sequential_storage::mock_flash::WriteCountCheck::Twice, None, true);
    let config = QueueConfig::new(MockFlash::FULL_FLASH_RANGE);
    let storage = QueueStorage::new(flash, config, NoCache::new());
    BufferedQueue::new(storage)
}

#[test]
fn enqueue_drain_pop() {
    block_on(async {
        let mut queue = make_queue();
        let mut scratch = [0u8; 64];
        let mut out = [0u8; 64];

        // Enqueue two items into RAM — no flash I/O yet.
        queue.enqueue(b"hello").unwrap();
        queue.enqueue(b"world").unwrap();
        assert_eq!(queue.ram_pending_count(), 2);

        // Drain RAM → flash.
        queue.drain_all(&mut scratch, false).await.unwrap();
        assert_eq!(queue.ram_pending_count(), 0);

        // Pop from flash in FIFO order.
        let data = queue.pop(&mut out, false).await.unwrap().unwrap();
        assert_eq!(data, b"hello");

        let data = queue.pop(&mut out, false).await.unwrap().unwrap();
        assert_eq!(data, b"world");

        assert!(queue.pop(&mut out, false).await.unwrap().is_none());
    });
}

#[test]
fn pop_reads_flash_before_ram() {
    block_on(async {
        let mut queue = make_queue();
        let mut out = [0u8; 64];

        // Commit "first" to flash directly via an aligned stack buffer (the mock flash
        // requires write buffers to be aligned to BYTES_PER_WORD = 4 bytes).
        let mut aligned = [0u8; 8];
        aligned[..5].copy_from_slice(b"first");
        queue.storage().push(&aligned[..5], false).await.unwrap();

        // Buffer "second" in RAM only.
        queue.enqueue(b"second").unwrap();

        // pop drains RAM to flash first, then pops the oldest flash item ("first").
        let data = queue.pop(&mut out, false).await.unwrap().unwrap();
        assert_eq!(data, b"first");

        // "second" was drained to flash during the previous pop; pop it now.
        let data = queue.pop(&mut out, false).await.unwrap().unwrap();
        assert_eq!(data, b"second");

        assert!(queue.pop(&mut out, false).await.unwrap().is_none());
    });
}

#[test]
fn enqueue_returns_err_when_ram_full() {
    let mut queue: BufferedQueue<MockFlash, NoCache, 16> = {
        let flash = MockFlash::new(
            sequential_storage::mock_flash::WriteCountCheck::Twice,
            None,
            true,
        );
        let config = QueueConfig::new(MockFlash::FULL_FLASH_RANGE);
        let storage = QueueStorage::new(flash, config, NoCache::new());
        BufferedQueue::new(storage)
    };

    // 16-byte ring: each item costs 2 (prefix) + data.len() bytes.
    // 3 items of 4 bytes = 3*6 = 18 bytes — won't all fit.
    queue.enqueue(b"aaaa").unwrap(); // 6 bytes used
    queue.enqueue(b"bbbb").unwrap(); // 12 bytes used
    assert!(queue.enqueue(b"cccc").is_err()); // 18 > 16: no room
}
