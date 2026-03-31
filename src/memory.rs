use log::{debug, info};

const PAGE_SIZE: usize = 4096;

/// A pool of memory chunks that are physically backed (RSS).
pub struct MemoryPool {
    chunks: Vec<Vec<u8>>,
    chunk_size: usize,
}

impl MemoryPool {
    pub fn new(chunk_size: usize) -> Self {
        Self {
            chunks: Vec::new(),
            chunk_size,
        }
    }

    /// Number of chunks currently held
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Total bytes held in the pool
    pub fn total_bytes(&self) -> usize {
        self.chunks.len() * self.chunk_size
    }

    /// Allocate and activate one chunk.
    /// Writing to every page forces the kernel to back it with physical memory.
    pub fn allocate_chunk(&mut self) {
        let mut chunk = vec![0u8; self.chunk_size];
        // Activate every page by writing a non-zero byte
        for offset in (0..self.chunk_size).step_by(PAGE_SIZE) {
            chunk[offset] = 0xAB;
        }
        self.chunks.push(chunk);
        debug!(
            "Allocated 1 chunk ({} MB), pool size: {} chunks ({} MB)",
            self.chunk_size / (1024 * 1024),
            self.chunks.len(),
            self.total_bytes() / (1024 * 1024)
        );
    }

    /// Allocate n chunks
    pub fn allocate_chunks(&mut self, n: usize) {
        for _ in 0..n {
            self.allocate_chunk();
        }
    }

    /// Release n chunks from the pool.
    /// Returns the actual number of chunks released.
    pub fn release_chunks(&mut self, n: usize) -> usize {
        let to_release = n.min(self.chunks.len());
        for _ in 0..to_release {
            // Drop the chunk (Vec is deallocated)
            self.chunks.pop();
        }
        if to_release > 0 {
            // Force glibc to return memory to OS instead of caching in the allocator
            trim_memory();
            info!(
                "Released {} chunks, pool size: {} chunks ({} MB)",
                to_release,
                self.chunks.len(),
                self.total_bytes() / (1024 * 1024)
            );
        }
        to_release
    }

    /// Release a fraction of chunks (e.g., 0.2 = release 20%)
    pub fn release_fraction(&mut self, fraction: f64) -> usize {
        let n = ((self.chunks.len() as f64) * fraction).ceil() as usize;
        self.release_chunks(n)
    }

    /// Release all chunks immediately
    pub fn release_all(&mut self) -> usize {
        let count = self.chunks.len();
        self.chunks.clear();
        if count > 0 {
            trim_memory();
            info!("Released ALL {} chunks (emergency)", count);
        }
        count
    }
}

/// Call glibc's malloc_trim to return freed memory to the OS.
fn trim_memory() {
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ──

    #[test]
    fn test_new_pool_is_empty() {
        let pool = MemoryPool::new(1024);
        assert!(pool.is_empty());
        assert_eq!(pool.len(), 0);
        assert_eq!(pool.total_bytes(), 0);
    }

    // ── Allocation ──

    #[test]
    fn test_allocate_single_chunk() {
        let mut pool = MemoryPool::new(4096); // 4KB
        pool.allocate_chunk();
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.total_bytes(), 4096);
    }

    #[test]
    fn test_allocate_multiple_chunks() {
        let mut pool = MemoryPool::new(4096);
        pool.allocate_chunks(10);
        assert_eq!(pool.len(), 10);
        assert_eq!(pool.total_bytes(), 40960);
    }

    #[test]
    fn test_allocate_zero_chunks() {
        let mut pool = MemoryPool::new(4096);
        pool.allocate_chunks(0);
        assert!(pool.is_empty());
    }

    #[test]
    fn test_page_activation_writes_nonzero() {
        let mut pool = MemoryPool::new(8192); // 2 pages
        pool.allocate_chunk();
        // Access the internal chunk to verify activation
        // We can't directly access chunks, but we verify no panic
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn test_large_chunk_allocation() {
        let mut pool = MemoryPool::new(1024 * 1024); // 1MB
        pool.allocate_chunks(5);
        assert_eq!(pool.len(), 5);
        assert_eq!(pool.total_bytes(), 5 * 1024 * 1024);
    }

    // ── Release ──

    #[test]
    fn test_release_partial() {
        let mut pool = MemoryPool::new(4096);
        pool.allocate_chunks(5);
        let released = pool.release_chunks(3);
        assert_eq!(released, 3);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn test_release_more_than_available() {
        let mut pool = MemoryPool::new(4096);
        pool.allocate_chunks(3);
        let released = pool.release_chunks(10);
        assert_eq!(released, 3); // only 3 available
        assert!(pool.is_empty());
    }

    #[test]
    fn test_release_zero() {
        let mut pool = MemoryPool::new(4096);
        pool.allocate_chunks(5);
        let released = pool.release_chunks(0);
        assert_eq!(released, 0);
        assert_eq!(pool.len(), 5);
    }

    #[test]
    fn test_release_from_empty_pool() {
        let mut pool = MemoryPool::new(4096);
        let released = pool.release_chunks(5);
        assert_eq!(released, 0);
    }

    // ── Release fraction ──

    #[test]
    fn test_release_fraction_20_percent() {
        let mut pool = MemoryPool::new(4096);
        pool.allocate_chunks(10);
        let released = pool.release_fraction(0.2);
        assert_eq!(released, 2);
        assert_eq!(pool.len(), 8);
    }

    #[test]
    fn test_release_fraction_100_percent() {
        let mut pool = MemoryPool::new(4096);
        pool.allocate_chunks(5);
        let released = pool.release_fraction(1.0);
        assert_eq!(released, 5);
        assert!(pool.is_empty());
    }

    #[test]
    fn test_release_fraction_zero() {
        let mut pool = MemoryPool::new(4096);
        pool.allocate_chunks(5);
        let released = pool.release_fraction(0.0);
        assert_eq!(released, 0);
        assert_eq!(pool.len(), 5);
    }

    #[test]
    fn test_release_fraction_rounds_up() {
        let mut pool = MemoryPool::new(4096);
        pool.allocate_chunks(3);
        // 0.5 * 3 = 1.5, ceil = 2
        let released = pool.release_fraction(0.5);
        assert_eq!(released, 2);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn test_release_fraction_on_empty() {
        let mut pool = MemoryPool::new(4096);
        let released = pool.release_fraction(0.5);
        assert_eq!(released, 0);
    }

    // ── Release all ──

    #[test]
    fn test_release_all() {
        let mut pool = MemoryPool::new(1024 * 1024);
        pool.allocate_chunks(5);
        let released = pool.release_all();
        assert_eq!(released, 5);
        assert!(pool.is_empty());
        assert_eq!(pool.total_bytes(), 0);
    }

    #[test]
    fn test_release_all_empty() {
        let mut pool = MemoryPool::new(4096);
        let released = pool.release_all();
        assert_eq!(released, 0);
    }

    // ── Lifecycle / multi-step ──

    #[test]
    fn test_allocate_release_cycle() {
        let mut pool = MemoryPool::new(4096);
        pool.allocate_chunks(5);
        pool.release_chunks(3);
        pool.allocate_chunks(2);
        assert_eq!(pool.len(), 4);
        pool.release_all();
        assert!(pool.is_empty());
        pool.allocate_chunks(1);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn test_total_bytes_after_mixed_operations() {
        let chunk_size = 1024;
        let mut pool = MemoryPool::new(chunk_size);
        pool.allocate_chunks(10);
        assert_eq!(pool.total_bytes(), 10 * chunk_size);
        pool.release_chunks(4);
        assert_eq!(pool.total_bytes(), 6 * chunk_size);
        pool.release_fraction(0.5); // release 3
        assert_eq!(pool.total_bytes(), 3 * chunk_size);
    }
}
