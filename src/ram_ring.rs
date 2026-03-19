/// A compact, fixed-capacity ring buffer for variable-length byte items.
///
/// Each item is stored as a 2-byte little-endian length prefix followed by the item's bytes,
/// so the usable capacity for data is `N - 2` bytes per item at most.
pub struct RamRing<const N: usize> {
    buf: [u8; N],
    read_pos: usize,
    write_pos: usize,
    /// Total bytes occupied (length prefixes + data).
    used: usize,
    item_count: usize,
}

impl<const N: usize> RamRing<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0u8; N],
            read_pos: 0,
            write_pos: 0,
            used: 0,
            item_count: 0,
        }
    }

    /// Number of items currently buffered.
    pub fn len(&self) -> usize {
        self.item_count
    }

    pub fn is_empty(&self) -> bool {
        self.item_count == 0
    }

    /// Bytes currently occupied (including length prefixes).
    pub fn bytes_used(&self) -> usize {
        self.used
    }

    /// Byte length of the oldest buffered item, or `None` if the ring is empty.
    pub fn oldest_len(&self) -> Option<usize> {
        if self.item_count == 0 {
            return None;
        }
        let lo = self.buf[self.read_pos] as usize;
        let hi = self.buf[(self.read_pos + 1) % N] as usize;
        Some(lo | (hi << 8))
    }

    /// Push an item into the ring.
    ///
    /// Returns `Err(())` if there is insufficient space or the item exceeds `u16::MAX` bytes.
    pub fn push(&mut self, data: &[u8]) -> Result<(), ()> {
        let len = data.len();
        if len > u16::MAX as usize {
            return Err(());
        }
        let total = 2 + len;
        if self.used + total > N {
            return Err(());
        }
        self.write_byte(len as u8);
        self.write_byte((len >> 8) as u8);
        for &b in data {
            self.write_byte(b);
        }
        self.used += total;
        self.item_count += 1;
        Ok(())
    }

    /// Copy the oldest item into `buf` and return a slice of the written bytes.
    ///
    /// Returns `None` if the ring is empty or `buf` is smaller than [`oldest_len`][Self::oldest_len].
    pub fn peek_into<'b>(&self, buf: &'b mut [u8]) -> Option<&'b [u8]> {
        let len = self.oldest_len()?;
        if buf.len() < len {
            return None;
        }
        let mut pos = (self.read_pos + 2) % N;
        for b in buf[..len].iter_mut() {
            *b = self.buf[pos];
            pos = (pos + 1) % N;
        }
        Some(&buf[..len])
    }

    /// Discard the oldest item without copying it. Does nothing if the ring is empty.
    pub fn discard_oldest(&mut self) {
        if let Some(len) = self.oldest_len() {
            self.read_pos = (self.read_pos + 2 + len) % N;
            self.used -= 2 + len;
            self.item_count -= 1;
        }
    }

    fn write_byte(&mut self, b: u8) {
        self.buf[self.write_pos] = b;
        self.write_pos = (self.write_pos + 1) % N;
    }
}

impl<const N: usize> Default for RamRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_peek_discard() {
        let mut ring: RamRing<64> = RamRing::new();
        assert!(ring.is_empty());

        ring.push(b"hello").unwrap();
        ring.push(b"world").unwrap();
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.oldest_len(), Some(5));

        let mut buf = [0u8; 32];
        assert_eq!(ring.peek_into(&mut buf), Some(b"hello".as_ref()));
        assert_eq!(ring.len(), 2); // peek does not consume

        ring.discard_oldest();
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.peek_into(&mut buf), Some(b"world".as_ref()));

        ring.discard_oldest();
        assert!(ring.is_empty());
        assert_eq!(ring.peek_into(&mut buf), None);
    }

    #[test]
    fn wrap_around() {
        // Use a tight buffer: 2 items of 3 bytes each = 2*(2+3)=10 bytes
        let mut ring: RamRing<10> = RamRing::new();
        ring.push(b"abc").unwrap();
        ring.push(b"def").unwrap();
        // Full — no room for another
        assert!(ring.push(b"x").is_err());

        let mut buf = [0u8; 8];
        ring.discard_oldest();
        ring.push(b"ghi").unwrap(); // wraps around
        assert_eq!(ring.peek_into(&mut buf), Some(b"def".as_ref()));
        ring.discard_oldest();
        assert_eq!(ring.peek_into(&mut buf), Some(b"ghi".as_ref()));
    }
}
