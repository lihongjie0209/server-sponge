use log::{debug, warn};

const PAGE_SIZE: usize = 4096;

/// MADV_POPULATE_WRITE (Linux 5.14+) — prefault and write to pages in one syscall.
/// Define manually in case the libc crate version doesn't include it yet.
#[cfg(target_os = "linux")]
const MADV_POPULATE_WRITE: libc::c_int = 23;

/// A single allocated memory region.
enum Chunk {
    /// Heap-allocated via Vec<u8>
    Heap(Vec<u8>),
    /// mmap-allocated (used for HugePages)
    Mmap {
        ptr: *mut u8,
        len: usize,
    },
}

// SAFETY: Chunk owns its memory exclusively; Send is safe.
unsafe impl Send for Chunk {}

impl Chunk {
    /// Allocate a chunk of the given size.
    /// When `huge` is true, attempts HugePage-backed mmap first, falling back to heap.
    fn allocate(size: usize, huge: bool) -> Self {
        if huge {
            // Try HugePage-backed mmap (2MB or 1GB pages)
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB,
                    -1,
                    0,
                )
            };
            if ptr != libc::MAP_FAILED {
                debug!("HugePage allocation: {} bytes", size);
                return Self::Mmap {
                    ptr: ptr as *mut u8,
                    len: size,
                };
            }
            warn!("HugePage allocation failed, falling back to regular heap");
        }
        // Regular heap allocation
        let mut v = vec![0u8; size];
        // Activate pages (fault them into RSS)
        activate_pages(v.as_mut_ptr(), size);
        Self::Heap(v)
    }

    /// Return the total usable size
    fn len(&self) -> usize {
        match self {
            Chunk::Heap(v) => v.len(),
            Chunk::Mmap { len, .. } => *len,
        }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        if let Chunk::Mmap { ptr, len } = *self {
            if !ptr.is_null() {
                unsafe {
                    libc::munmap(ptr as *mut libc::c_void, len);
                }
            }
        }
        // Heap variant drops Vec<u8> automatically
    }
}

/// Activate memory pages so they become resident (RSS).
/// Uses MADV_POPULATE_WRITE (Linux 5.14+) when available, with fallback to a write loop.
fn activate_pages(ptr: *mut u8, len: usize) {
    #[cfg(target_os = "linux")]
    {
        let ret = unsafe { libc::madvise(ptr as *mut libc::c_void, len, MADV_POPULATE_WRITE) };
        if ret == 0 {
            debug!("Activated {} bytes via MADV_POPULATE_WRITE", len);
            return;
        }
        // Fall through to manual write loop
    }
    // Fallback: touch every page to force physical backing
    let mut offset = 0;
    while offset < len {
        unsafe {
            *ptr.add(offset) = 0xAB;
        }
        offset += PAGE_SIZE;
    }
    debug!("Activated {} bytes via write loop ({} pages)", len, len / PAGE_SIZE);
}

/// Activate pages in a Vec<u8>
fn activate_vec(v: &mut Vec<u8>) {
    activate_pages(v.as_mut_ptr(), v.len());
}

/// A pool of memory chunks that are physically backed (RSS).
pub struct MemoryPool {
    chunks: Vec<Chunk>,
    chunk_size: usize,
    use_hugepages: bool,
}

impl MemoryPool {
    /// Create a new pool. `chunk_size` is the size of each allocation chunk.
    pub fn new(chunk_size: usize, use_hugepages: bool) -> Self {
        Self {
            chunks: Vec::new(),
            chunk_size,
            use_hugepages,
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
    pub fn allocate_chunk(&mut self) {
        let chunk = Chunk::allocate(self.chunk_size, self.use_hugepages);
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
            self.chunks.pop();
        }
        if to_release > 0 {
            // For heap chunks, encourage the allocator to return memory to the OS
            trim_memory();
            debug!(
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
            debug!("Released ALL {} chunks (emergency)", count);
        }
        count
    }
}

/// Return freed memory to the OS (best-effort, allocator-specific).
///
/// On glibc: calls malloc_trim(0) to reclaim cached free pages.
/// On musl: large allocations (>= 128 KB) are backed by mmap and returned
/// to the OS via munmap on free — no explicit trim is needed.
fn trim_memory() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
    #[cfg(target_env = "musl")]
    {
        let _ = 0;
    }
}

// ── Helpers for auto chunk sizing ──

/// Calculate a reasonable chunk size based on system memory.
/// Returns size in MB, clamped to [4, 256].
pub fn auto_chunk_size_mb(total_mb: u64) -> usize {
    // Target: each chunk is ~1% of total memory, but keep it reasonable
    let auto = (total_mb as f64 * 0.01).round() as usize;
    auto.max(4).min(256)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ──

    #[test]
    fn test_new_pool_is_empty() {
        let pool = MemoryPool::new(1024, false);
        assert!(pool.is_empty());
        assert_eq!(pool.len(), 0);
        assert_eq!(pool.total_bytes(), 0);
    }

    // ── Allocation ──

    #[test]
    fn test_allocate_single_chunk() {
        let mut pool = MemoryPool::new(4096, false);
        pool.allocate_chunk();
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.total_bytes(), 4096);
    }

    #[test]
    fn test_allocate_multiple_chunks() {
        let mut pool = MemoryPool::new(4096, false);
        pool.allocate_chunks(10);
        assert_eq!(pool.len(), 10);
        assert_eq!(pool.total_bytes(), 40960);
    }

    #[test]
    fn test_allocate_zero_chunks() {
        let mut pool = MemoryPool::new(4096, false);
        pool.allocate_chunks(0);
        assert!(pool.is_empty());
    }

    #[test]
    fn test_page_activation_no_panic() {
        let mut pool = MemoryPool::new(8192, false);
        pool.allocate_chunk();
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn test_large_chunk_allocation() {
        let mut pool = MemoryPool::new(1024 * 1024, false);
        pool.allocate_chunks(5);
        assert_eq!(pool.len(), 5);
        assert_eq!(pool.total_bytes(), 5 * 1024 * 1024);
    }

    // ── Release ──

    #[test]
    fn test_release_partial() {
        let mut pool = MemoryPool::new(4096, false);
        pool.allocate_chunks(5);
        let released = pool.release_chunks(3);
        assert_eq!(released, 3);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn test_release_more_than_available() {
        let mut pool = MemoryPool::new(4096, false);
        pool.allocate_chunks(3);
        let released = pool.release_chunks(10);
        assert_eq!(released, 3);
        assert!(pool.is_empty());
    }

    #[test]
    fn test_release_zero() {
        let mut pool = MemoryPool::new(4096, false);
        pool.allocate_chunks(5);
        let released = pool.release_chunks(0);
        assert_eq!(released, 0);
        assert_eq!(pool.len(), 5);
    }

    #[test]
    fn test_release_from_empty_pool() {
        let mut pool = MemoryPool::new(4096, false);
        let released = pool.release_chunks(5);
        assert_eq!(released, 0);
    }

    #[test]
    fn test_release_fraction_20_percent() {
        let mut pool = MemoryPool::new(4096, false);
        pool.allocate_chunks(10);
        let released = pool.release_fraction(0.2);
        assert_eq!(released, 2);
        assert_eq!(pool.len(), 8);
    }

    #[test]
    fn test_release_fraction_100_percent() {
        let mut pool = MemoryPool::new(4096, false);
        pool.allocate_chunks(5);
        let released = pool.release_fraction(1.0);
        assert_eq!(released, 5);
        assert!(pool.is_empty());
    }

    #[test]
    fn test_release_fraction_zero() {
        let mut pool = MemoryPool::new(4096, false);
        pool.allocate_chunks(5);
        let released = pool.release_fraction(0.0);
        assert_eq!(released, 0);
        assert_eq!(pool.len(), 5);
    }

    #[test]
    fn test_release_fraction_rounds_up() {
        let mut pool = MemoryPool::new(4096, false);
        pool.allocate_chunks(3);
        let released = pool.release_fraction(0.5);
        assert_eq!(released, 2);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn test_release_fraction_on_empty() {
        let mut pool = MemoryPool::new(4096, false);
        let released = pool.release_fraction(0.5);
        assert_eq!(released, 0);
    }

    #[test]
    fn test_release_all() {
        let mut pool = MemoryPool::new(1024 * 1024, false);
        pool.allocate_chunks(5);
        let released = pool.release_all();
        assert_eq!(released, 5);
        assert!(pool.is_empty());
        assert_eq!(pool.total_bytes(), 0);
    }

    #[test]
    fn test_release_all_empty() {
        let mut pool = MemoryPool::new(4096, false);
        let released = pool.release_all();
        assert_eq!(released, 0);
    }

    #[test]
    fn test_allocate_release_cycle() {
        let mut pool = MemoryPool::new(4096, false);
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
        let mut pool = MemoryPool::new(chunk_size, false);
        pool.allocate_chunks(10);
        assert_eq!(pool.total_bytes(), 10 * chunk_size);
        pool.release_chunks(4);
        assert_eq!(pool.total_bytes(), 6 * chunk_size);
        pool.release_fraction(0.5);
        assert_eq!(pool.total_bytes(), 3 * chunk_size);
    }

    #[test]
    fn test_auto_chunk_size() {
        // 2 GB system → 1% = ~20 MB
        let c = auto_chunk_size_mb(2048);
        assert_eq!(c, 20);

        // 512 MB system → 1% = ~5 MB, clamped min 4
        let c = auto_chunk_size_mb(512);
        assert!(c >= 4, "{}", c);

        // 64 GB system → 1% = 655 MB, clamped max 256
        let c = auto_chunk_size_mb(65536);
        assert_eq!(c, 256);

        // Very small system
        let c = auto_chunk_size_mb(256);
        assert_eq!(c, 4);
    }

    #[test]
    fn test_chunk_allocate_and_activate() {
        let c = Chunk::allocate(8192, false);
        assert_eq!(c.len(), 8192);
    }

    #[test]
    fn test_chunk_drop_doesnt_leak() {
        // Smoke test: allocate and drop many chunks to check for crashes
        for _ in 0..100 {
            let c = Chunk::allocate(4096, false);
            drop(c);
        }
    }
}
