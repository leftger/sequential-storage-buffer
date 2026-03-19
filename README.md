# sequential-storage-buffer

A RAM-buffered queue on top of [`sequential-storage`](https://github.com/tweedegolf/sequential-storage) for NOR flash.

## The problem

NOR flash is slow. A page erase can take hundreds of milliseconds, and even a word write costs microseconds. If a producer — a sensor, a UART, an ADC DMA transfer — emits data faster than the flash interface can commit it, you need somewhere to put items while the flash catches up.

`sequential-storage-buffer` adds a fixed-size RAM ring buffer in front of `QueueStorage`. The fast path (`enqueue`) is synchronous and never touches flash. The slow path (`drain_all`) commits the ring to flash asynchronously, typically from a low-priority task.

## Usage

Add the dependency:

```toml
[dependencies]
sequential-storage-buffer = { git = "https://github.com/leftger/sequential-storage-buffer" }
```

### Single-task use — `BufferedQueue`

```rust
use sequential_storage_buffer::{BufferedQueue, OverflowPolicy};
use sequential_storage::{cache::NoCache, queue::{QueueConfig, QueueStorage}};

// Wrap an existing QueueStorage with a 512-byte RAM ring.
let storage = QueueStorage::new(flash, QueueConfig::new(0..FLASH_SIZE), NoCache::new());
let mut queue: BufferedQueue<_, _, 512> = BufferedQueue::new(storage);

// Fast path — synchronous, never touches flash.
queue.enqueue(&sensor_sample, OverflowPolicy::Err)?;

// Slow path — call from a low-priority task or on a timer.
queue.drain_all(&mut scratch, false).await?;

// Read path — drains any pending RAM items to flash first, then pops.
if let Some(data) = queue.pop(&mut buf, false).await? {
    // process data
}
```

### Overflow policies

When the RAM ring is full, `enqueue` behaviour is controlled by [`OverflowPolicy`]:

| Policy | Behaviour |
|---|---|
| `OverflowPolicy::Err` | Returns `Err(())` — caller decides whether to drop or drain first |
| `OverflowPolicy::DiscardOldest` | Evicts the oldest buffered item(s) to make room; always succeeds if the item fits in the ring at all |

### ISR-safe use — `SharedRamRing` (requires `embassy` feature)

When data arrives in an interrupt handler, use `SharedRamRing`. It wraps the ring in a
`CriticalSectionRawMutex` blocking mutex so `enqueue` is safe from any context, and an Embassy
[`Signal`](https://docs.embassy.dev/embassy-sync/git/default/signal/struct.Signal.html) wakes
the drain task the instant data arrives — no polling timer needed.

```toml
[dependencies]
sequential-storage-buffer = { ..., features = ["embassy"] }
```

```rust
use sequential_storage_buffer::{OverflowPolicy, shared::SharedRamRing};

// Place in a static — SharedRamRing::new() is const.
static RING: SharedRamRing<512> = SharedRamRing::new();

// In an interrupt handler (or any context):
#[interrupt]
fn UART_RX() {
    let byte = read_uart_byte();
    RING.enqueue(&[byte], OverflowPolicy::DiscardOldest);
}

// In an Embassy drain task:
#[embassy_executor::task]
async fn drain_task(mut storage: QueueStorage<Flash, NoCache>) {
    let mut scratch = [0u8; 64];
    loop {
        // Sleeps until RING.enqueue() signals; then drains everything to flash.
        RING.wait_and_drain_all(&mut storage, &mut scratch, false).await.unwrap();
    }
}
```

The critical section is held only for the brief ring peek/discard. The slow flash write runs
outside it, so interrupt latency is not impacted.

## Ordering

FIFO ordering is always preserved. Flash-committed items are older than RAM-buffered ones.
`pop` and `peek` drain any pending RAM items to flash first, then read from the flash queue.

## Power-fail safety

Items in the RAM ring that have not been drained to flash **will be lost** on power loss.
Items that have been drained carry the same power-fail guarantees as `sequential-storage` itself
(CRC-protected headers, recoverable after an interrupted write or erase).

## Introspection

```rust
// BufferedQueue and SharedRamRing both expose:
BufferedQueue::<_, _, 512>::ram_capacity_bytes() // → 512 (const)
queue.ram_free_bytes()                            // → bytes available
queue.ram_bytes_used()                            // → bytes occupied
queue.ram_pending_count()                         // → items not yet in flash
queue.oldest_ram_item_len()                       // → Some(n) or None
```

## Features

| Feature | Enables |
|---|---|
| `embassy` | [`SharedRamRing`] — ISR-safe enqueue with Embassy Signal wakeup |
| `defmt` | `defmt::Format` on public types; forwards `sequential-storage/defmt` |
| `test-utils` | Re-exports `sequential-storage`'s `MockFlash` for use in downstream tests |

## Relation to `sequential-storage`

This crate is a thin layer on top of `sequential-storage`. It only uses the public
`QueueStorage` API — no internal access. The two crates version independently; pin
`sequential-storage` to a specific version range if you need stability across both.
