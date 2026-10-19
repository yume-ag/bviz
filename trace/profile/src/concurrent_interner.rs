//! A deduplicating string interner interned concurrently from many threads. Its
//! dedup index is global — a string interned on any thread resolves to one
//! handle for every thread — so the [`Str`] handles it hands out have identity
//! equal to text identity across the whole pool. Sealing yields a text pool
//! that [`crate::Profile::str`] indexes directly.

use std::cell::UnsafeCell;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bviz_parallel::worker_count;
use crossbeam_utils::CachePadded;
use hashbrown::HashTable;

use crate::model::Str;

/// Backs `[ptr, ptr + len)` with ordinary 4 KB pages, best-effort. The arena is
/// a large anonymous mapping first-touched concurrently by the parse workers;
/// under system THP=always, fault-time or khugepaged huge-page collapse of it
/// triggers compaction stalls that serialize those first touches.
#[cfg(target_os = "linux")]
fn advise_no_thp(ptr: *mut std::os::raw::c_void, len: usize) {
    use std::os::raw::{c_int, c_void};

    // Linux `asm-generic/mman-common.h` MADV_NOHUGEPAGE (architecture-independent).
    const MADV_NOHUGEPAGE: c_int = 15;

    unsafe extern "C" {
        fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
    }

    // SAFETY: `[ptr, len)` names the just-allocated arena mapping; madvise only
    // sets page-backing policy and neither reads, writes, nor aliases the
    // buffer, so it cannot violate the arena's aliasing invariants. Advisory: a
    // failure (unaligned base from the allocator, a kernel without THP) leaves
    // ordinary backing, so the result is ignored.
    unsafe {
        let _ = madvise(ptr, len, MADV_NOHUGEPAGE);
    }
}

#[cfg(not(target_os = "linux"))]
fn advise_no_thp(_ptr: *mut std::os::raw::c_void, _len: usize) {}

/// Hashes an interner key. Non-cryptographic: the keys are short profile
/// strings interned tens of millions of times, where the default SipHash is the
/// dominant lookup cost. That trades resistance to hash flooding for speed: a
/// profile crafted to collide can slow only its own load.
fn intern_hash(s: &str) -> u64 {
    use std::hash::BuildHasher;
    rustc_hash::FxBuildHasher.hash_one(s)
}

/// Bytes handed out per page-cursor bump. Large enough that the atomic bump is
/// rare — one per this many bytes of distinct text per worker — yet small
/// enough that each worker's unused tail on its final page (and the slack left
/// when a string doesn't fit the current page) stays a negligible share of the
/// pool. Far above any Bazel profile token (target labels, paths, mnemonics run
/// to hundreds of bytes), so the dedicated-chunk path for a longer string is a
/// rare fallback, not the common case.
const PAGE_SIZE: usize = 256 * 1024;

/// Shard-count bounds. Too few shards and disjoint strings contend on a lock;
/// too many wastes an empty table per shard. A power of two so the shard index
/// is a shift of the hash's high bits.
const MIN_SHARDS: usize = 16;
const MAX_SHARDS: usize = 256;

/// Slots in a [`Writer`]'s direct-mapped read cache, keyed by the low hash bits.
/// A power of two so the index is a mask. At 16 bytes/slot this is 16 KB — small
/// enough to stay resident in L1 alongside the parse's other hot data, which is
/// what makes the lock-free hit pay off: a larger table (measured at 256 KB)
/// evicts the shard and arena lines it means to avoid touching and loses to no
/// cache at all.
const CACHE_SLOTS: usize = 1024;

/// The handle of the empty string, which needs no bytes. Offset 1 keeps it off
/// the placeholder at offset 0 (the `OptStr` absence encoding), and its range is
/// empty, so it resolves to "" before any page is grabbed.
const EMPTY: Str = Str::new(1, 0);

/// Why a [`ConcurrentInterner`]'s pool cannot hold the distinct text given it.
#[derive(Debug, PartialEq)]
pub(crate) enum PoolOverflow {
    /// A pool sized for the text would reach past the offsets a `u32` handle
    /// indexes.
    TooLarge,
    /// The distinct text outgrew the pool it was sized for.
    Exhausted,
}

/// The one contiguous, zero-initialized text buffer, written concurrently by
/// the [`Writer`]s into disjoint page ranges. Byte 0 is the pool placeholder
/// (it is never in any page), so no live handle has offset 0 and `OptStr`
/// absence holds; unwritten within-page slack stays NUL, so the sealed prefix
/// is valid UTF-8.
struct Arena {
    buf: Box<[UnsafeCell<u8>]>,
    cap: usize,
    page_size: usize,
    /// Next unallocated byte offset, starting at 1 so byte 0 is reserved as the
    /// placeholder. Every byte below this is in a grabbed page or the
    /// placeholder, hence initialized — so its value at seal is exactly the used
    /// pool length.
    page_cursor: AtomicUsize,
    /// Set once a reservation finds no room, after which the pool is missing
    /// text its handles name and cannot be sealed.
    exhausted: AtomicBool,
}

impl Arena {
    /// Takes `want` bytes from the cursor, or `least` where the pool has no room
    /// left for the larger claim, reporting the offset taken and how much of it
    /// was granted. Page granularity is an optimization over the pool's text
    /// budget rather than a claim on it, so a writer the pool cannot give a
    /// whole page takes only the bytes it needs and the pool runs out only when
    /// the distinct text it is sized for does. `None`, marking the pool
    /// exhausted, when not even `least` fits.
    fn reserve(&self, want: usize, least: usize) -> Option<(usize, usize)> {
        let mut start = self.page_cursor.load(Ordering::Relaxed);
        loop {
            let take = if start + want <= self.cap {
                want
            } else if start + least <= self.cap {
                least
            } else {
                self.exhausted.store(true, Ordering::Relaxed);
                return None;
            };
            match self.page_cursor.compare_exchange_weak(
                start,
                start + take,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some((start, take)),
                Err(current) => start = current,
            }
        }
    }

    /// Copies `bytes` into `[off, off + bytes.len())`.
    ///
    /// SAFETY: the caller must have reserved `[off, off + bytes.len())` from the
    /// cursor — a page it holds, or a dedicated chunk it just took — so the range
    /// is disjoint from every other allocation (no thread writes those bytes) and
    /// in bounds (each reservation is checked against `cap`).
    unsafe fn write(&self, off: usize, bytes: &[u8]) {
        unsafe {
            let dst = (self.buf.as_ptr() as *mut u8).add(off);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
    }

    /// The interned bytes of a handle. Only handles already published into a
    /// shard table are resolved, and their bytes were written before that
    /// publish and are never rewritten, so the read never races a write.
    fn str(&self, h: Str) -> &str {
        let r = h.range();
        debug_assert!(
            r.end <= self.page_cursor.load(Ordering::Relaxed),
            "handle {h:?} reaches past the reserved pool"
        );
        // SAFETY: r lies in a written, immutable region; the bytes there are
        // valid UTF-8 (a copied token).
        unsafe {
            let p = (self.buf.as_ptr() as *const u8).add(r.start);
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(p, r.len()))
        }
    }
}

/// The mutable-phase interner: shared behind `&self` across worker threads,
/// each holding a [`Writer`]. Sealed with [`Self::into_text`] into the text
/// pool that resolves its handles.
pub(crate) struct ConcurrentInterner {
    /// Each shard on its own cache line, so a shard's write-lock traffic can't
    /// invalidate a neighbouring shard's line held on another core.
    shards: Box<[CachePadded<RwLock<HashTable<Str>>>]>,
    /// `64 - log2(shards.len())`: shifting an FxHash right by this yields the
    /// shard index from its high bits (FxHash 2.x finalizes, so the high bits
    /// are well mixed).
    shard_shift: u32,
    arena: Arena,
}

// SAFETY: the only shared mutation is the arena's disjoint per-page byte writes
// (each byte written once by the single Writer holding that page) and the
// lock-guarded shard tables; no two threads touch the same location without
// synchronization.
unsafe impl Sync for ConcurrentInterner {}

impl ConcurrentInterner {
    /// An interner whose pool is bounded by `text_bytes` of distinct text and
    /// whose index is sized for `distinct` strings. `text_bytes` is the caller's
    /// bound on the total distinct bytes; the buffer adds slack over it so that
    /// writers keep taking whole pages, which [`Arena::reserve`] gives up on
    /// rather than exceeding the pool. [`PoolOverflow::TooLarge`] when that
    /// buffer would outgrow `u32` handles.
    pub(crate) fn new(
        text_bytes: usize,
        distinct: usize,
    ) -> Result<ConcurrentInterner, PoolOverflow> {
        // One byte for the placeholder, the bound itself, a small fraction for
        // within-page slack, and a page per worker the host can run, which is
        // what a parse sized to the host spends on final-page tails.
        let cap = 1 + text_bytes + text_bytes / 16 + (worker_count() + 2) * PAGE_SIZE;
        // The sealed pool indexes handles with u32, so every reserved offset
        // must fit u32; refusing a larger pool here means `off as u32` in
        // `alloc` can never silently truncate a handle.
        if cap > u32::MAX as usize {
            return Err(PoolOverflow::TooLarge);
        }
        Ok(Self::with_pool(cap, distinct))
    }

    /// An interner over a `cap`-byte buffer, whose index is sized for
    /// `distinct` strings.
    fn with_pool(cap: usize, distinct: usize) -> ConcurrentInterner {
        // The parallel parse's writer count: shard fan-out tracks the threads
        // that actually intern, i.e. physical cores.
        let par = worker_count();
        let shard_count = (4 * par).next_power_of_two().clamp(MIN_SHARDS, MAX_SHARDS);
        let shard_shift = u64::BITS - shard_count.trailing_zeros();
        let shards = (0..shard_count)
            .map(|_| {
                CachePadded::new(RwLock::new(HashTable::with_capacity(
                    distinct / shard_count + 1,
                )))
            })
            .collect();

        let buf: Box<[u8]> = vec![0u8; cap].into_boxed_slice();
        advise_no_thp(buf.as_ptr() as *mut std::os::raw::c_void, cap);
        // SAFETY: `UnsafeCell<u8>` is `repr(transparent)` over `u8` — identical
        // size and alignment — so reinterpreting the zeroed buffer as a slice of
        // cells preserves its layout and lets disjoint page writes go through
        // `&self`.
        let buf: Box<[UnsafeCell<u8>]> =
            unsafe { Box::from_raw(Box::into_raw(buf) as *mut [UnsafeCell<u8>]) };

        ConcurrentInterner {
            shards,
            shard_shift,
            arena: Arena {
                buf,
                cap,
                page_size: PAGE_SIZE,
                page_cursor: AtomicUsize::new(1),
                exhausted: AtomicBool::new(false),
            },
        }
    }

    /// Faults in this worker's page-aligned share of the arena's first
    /// `min(hint_bytes, cap)` bytes with writable pages, so the first touches
    /// interning would otherwise take one 4 KB page at a time in the hot loop
    /// happen up front. Each parse worker calls it as `shard` of `shards`
    /// before its receive loop, so the fault sweep overlaps the producer's
    /// initial inflate — the window workers would otherwise spend blocked
    /// waiting for the first chunk — rather than a dedicated pass on the
    /// sequential path. `hint_bytes` is advisory: it governs only how many
    /// pages to touch, never the buffer's size, so a wrong hint costs only
    /// bandwidth — too large faults never-written pages, too small leaves a
    /// lazy tail — and never correctness.
    ///
    /// Only MADV_POPULATE_WRITE is used: it faults absent pages in writable and
    /// leaves any already-present page's bytes untouched, so a worker may run it
    /// while others intern into the shared arena. A page another worker has
    /// already written keeps its bytes; an absent page it zero-fills names no
    /// live handle yet. A kernel without the advice (before 5.14) leaves the
    /// arena to fault lazily during parse.
    #[cfg(target_os = "linux")]
    pub(crate) fn prefault_shard(&self, shard: usize, shards: usize, hint_bytes: usize) {
        use std::os::raw::{c_int, c_void};

        // Linux `asm-generic/mman-common.h` (Linux 5.14+): fault the range's
        // page tables in and populate them with writable anonymous pages.
        const MADV_POPULATE_WRITE: c_int = 23;

        unsafe extern "C" {
            fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
            fn getpagesize() -> c_int;
        }

        // The kernel's base page size — the alignment madvise requires. Queried
        // rather than assumed 4 KB so the fast path still applies on kernels
        // with larger base pages (arm64 16/64 KB); NOHUGEPAGE suppresses only
        // transparent huge pages, not the base size.
        // SAFETY: getpagesize reads a cached value and has no preconditions.
        let page = unsafe { getpagesize() } as usize;
        let len = hint_bytes.min(self.arena.cap);
        let shards = shards.max(1);
        // Page-align the per-shard span so each shard's start is a page boundary
        // (the arena base is page-aligned): madvise requires it, and it keeps
        // two shards off a shared boundary page.
        let per = len.div_ceil(shards).next_multiple_of(page);
        let start = shard * per;
        if start >= len {
            return;
        }
        let sub = (start + per).min(len) - start;
        let addr = (self.arena.buf.as_ptr() as usize + start) as *mut c_void;
        // SAFETY: `[start, start + sub)` lies within `[0, cap)` of the live
        // `buf` allocation (the `.min(cap)` clamp and the split bound it).
        // MADV_POPULATE_WRITE only faults page tables and modifies no present
        // page's bytes, so it neither races nor corrupts the concurrent
        // interning writes through `&self`'s UnsafeCell buffer.
        unsafe {
            let _ = madvise(addr, sub, MADV_POPULATE_WRITE);
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn prefault_shard(&self, _shard: usize, _shards: usize, _hint_bytes: usize) {}

    /// A per-worker handle owning a current page, so its hot path does no
    /// atomics on a hit and one page-cursor bump only when a miss overflows the
    /// page. The borrow makes the mutable phase explicit.
    pub(crate) fn writer(&self) -> Writer<'_> {
        // Every slot starts as the sentinel (0, Str::new(0, 0)); a real handle
        // never has offset 0, so an empty slot can never be mistaken for a hit.
        Writer {
            interner: self,
            page_pos: 0,
            page_end: 0,
            cache: vec![(0, Str::new(0, 0)); CACHE_SLOTS].into_boxed_slice(),
        }
    }

    /// The interned bytes for a handle this interner produced, for
    /// single-threaded resolution during sealing. Safe only for a handle
    /// already published into a shard table (its bytes were written before the
    /// publish and are never rewritten).
    pub(crate) fn resolve(&self, h: Str) -> &str {
        self.arena.str(h)
    }

    /// The number of distinct strings interned so far — the summed occupancy of
    /// the shard tables. Read at seal to size the downstream aggregations that
    /// key on distinct text, so they reserve once instead of rehashing up from
    /// empty.
    pub(crate) fn distinct(&self) -> usize {
        self.shards.iter().map(|s| s.read().unwrap().len()).sum()
    }

    /// Whether a string has found the pool full, so that sealing will fail.
    pub(crate) fn exhausted(&self) -> bool {
        self.arena.exhausted.load(Ordering::Relaxed)
    }

    /// Consumes the interner and returns its text pool, dropping the shard
    /// index. Every byte below the page cursor is written or placeholder/slack
    /// NUL, all valid UTF-8, so the pool resolves the handles this interner
    /// produced. [`PoolOverflow::Exhausted`] when some string found no room.
    pub(crate) fn into_text(self) -> Result<String, PoolOverflow> {
        if self.exhausted() {
            return Err(PoolOverflow::Exhausted);
        }
        let ConcurrentInterner { arena, .. } = self;
        let len = arena.page_cursor.load(Ordering::Relaxed);
        let Arena { buf, cap, .. } = arena;
        let raw = Box::into_raw(buf) as *mut u8;
        // SAFETY: `raw`/`cap` are the allocation `new` made as a `cap`-byte
        // buffer; `len <= cap` and every byte in `[0, len)` is initialized.
        let vec = unsafe { Vec::from_raw_parts(raw, len, cap) };
        // SAFETY: those bytes are the placeholder, copied UTF-8 tokens, and NUL
        // slack — all valid UTF-8.
        Ok(unsafe { String::from_utf8_unchecked(vec) })
    }
}

/// A worker's interning handle into a [`ConcurrentInterner`]. Holds the current
/// bump page so repeated interning touches shared state only on a miss (a shard
/// lock) or a page overflow (the atomic cursor).
pub(crate) struct Writer<'a> {
    interner: &'a ConcurrentInterner,
    /// Next free absolute byte offset in the current page, and the page's end.
    /// Both 0 initially so the first intern grabs a page.
    page_pos: usize,
    page_end: usize,
    /// Direct-mapped `(hash, handle)` cache of recently interned strings, so a
    /// repeat resolves without a shard lock. Holds only published global
    /// handles, so a hit is always the same handle every worker shares.
    cache: Box<[(u64, Str)]>,
}

impl Writer<'_> {
    /// Interns `s`, returning the one handle every worker shares for it. A
    /// worker-local cache resolves a repeat with no lock at all — the common
    /// case, since a profile repeats most strings. On a cache miss it falls to
    /// the shared shard index and records the handle for next time. A string
    /// the exhausted pool has no room for gets the empty string's handle, and
    /// the pool then refuses to seal.
    pub(crate) fn intern(&mut self, s: &str) -> Str {
        let hash = intern_hash(s);
        let arena = &self.interner.arena;
        let idx = hash as usize & (CACHE_SLOTS - 1);
        let slot = self.cache[idx];
        // A real handle never has offset 0, so rejecting the sentinel keeps an
        // empty slot — and a zero-hash `""` query that would else match it —
        // out of the hit path; every populated slot passes this compare.
        if slot.1 != Str::new(0, 0) && slot.0 == hash && arena.str(slot.1) == s {
            return slot.1;
        }
        let handle = self.intern_shard(hash, s);
        self.cache[idx] = (hash, handle);
        handle
    }

    /// Resolves `s` through the shared shard index, interning it on a miss. A hit
    /// takes only the shard's read lock, so concurrent interns of other strings
    /// in the shard don't serialize. A miss takes the write lock, re-checks
    /// (another worker may have inserted `s` in the gap), then bump-copies the
    /// bytes into this worker's page and publishes the handle. Equal strings hash
    /// to the same shard, so the write lock serializes them and inserts once; the
    /// bytes are written under that lock before the handle is published, and
    /// every later observer takes the same lock, so it sees them.
    fn intern_shard(&mut self, hash: u64, s: &str) -> Str {
        let shard = (hash >> self.interner.shard_shift) as usize;
        let arena = &self.interner.arena;
        if let Some(&handle) = self.interner.shards[shard]
            .read()
            .unwrap()
            .find(hash, |&h| arena.str(h) == s)
        {
            return handle;
        }
        let mut table = self.interner.shards[shard].write().unwrap();
        if let Some(&handle) = table.find(hash, |&h| arena.str(h) == s) {
            return handle;
        }
        let Some(handle) = self.alloc(s) else {
            return EMPTY;
        };
        table.insert_unique(hash, handle, |&h| intern_hash(arena.str(h)));
        handle
    }

    /// Whether the pool this writer interns into is exhausted; see
    /// [`ConcurrentInterner::exhausted`].
    pub(crate) fn pool_exhausted(&self) -> bool {
        self.interner.exhausted()
    }

    /// Reserves `s`'s bytes from the cursor and copies them in, returning the
    /// handle. A string that fits the current page bumps through it; one that
    /// needs a fresh page adopts whatever [`Arena::reserve`] grants; one
    /// larger than a page takes a dedicated exact-size chunk without disturbing
    /// the writer's current page, so subsequent small strings keep filling it.
    /// `None` when the pool has no room left for `s`.
    fn alloc(&mut self, s: &str) -> Option<Str> {
        let arena = &self.interner.arena;
        let n = s.len();
        if n == 0 {
            return Some(EMPTY);
        }
        let off = if n > arena.page_size {
            arena.reserve(n, n)?.0
        } else {
            if self.page_pos + n > self.page_end {
                let (start, granted) = arena.reserve(arena.page_size, n)?;
                self.page_pos = start;
                self.page_end = start + granted;
            }
            let off = self.page_pos;
            self.page_pos += n;
            off
        };
        // SAFETY: `[off, off + n)` was reserved from the cursor just above (a
        // page this Writer holds or a dedicated chunk), disjoint from every
        // other allocation and within `cap` by the checked reservation.
        unsafe { arena.write(off, s.as_bytes()) };
        debug_assert!(off != 0, "offset 0 is the pool placeholder");
        Some(Str::new(off as u32, n as u32))
    }
}

#[cfg(test)]
impl ConcurrentInterner {
    /// A `new` interner overriding the page size, so a test can straddle page
    /// boundaries and exceed a page cheaply.
    fn with_page_size(text_bytes: usize, distinct: usize, page_bytes: usize) -> ConcurrentInterner {
        let mut interner = ConcurrentInterner::new(text_bytes, distinct).expect("a small pool");
        interner.arena.page_size = page_bytes;
        interner
    }

    /// An interner over exactly `cap` bytes in pages of `page_bytes`, so a test
    /// can run the pool out.
    pub(crate) fn with_exact_pool(cap: usize, page_bytes: usize) -> ConcurrentInterner {
        let mut interner = ConcurrentInterner::with_pool(cap, 0);
        interner.arena.page_size = page_bytes;
        interner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn single_thread_interns_and_dedups() {
        let seq = [
            "CppCompile",
            "//src/foo:bar",
            "CppCompile",
            "Javac",
            "//src/foo:bar",
            "//a/b/c/d:e",
            "Javac",
            "CppCompile",
        ];
        let interner = ConcurrentInterner::new(256, 8).expect("a small pool");
        let handles: Vec<Str> = {
            let mut w = interner.writer();
            seq.iter().map(|s| w.intern(s)).collect()
        };
        let text = interner.into_text().expect("the pool holds its text");

        assert_eq!(text.as_bytes()[0], 0, "offset 0 is the placeholder");
        let resolved: Vec<&str> = handles.iter().map(|&h| &text[h.range()]).collect();
        assert_eq!(resolved, seq);
        // Repeats collapse to one handle.
        assert_eq!(handles[0], handles[2]);
        assert_eq!(handles[0], handles[7]);
        assert_eq!(handles[1], handles[4]);
        assert_eq!(handles[3], handles[6]);
        let distinct: std::collections::HashSet<Str> = handles.iter().copied().collect();
        assert_eq!(distinct.len(), 4);
        for &h in &distinct {
            assert!(h.range().start != 0);
        }
    }

    /// The identity the by-text pass would otherwise reconstruct: across all
    /// threads, one handle per distinct text and one text per distinct handle.
    #[test]
    fn multi_thread_yields_one_handle_per_text() {
        // The arena reserves final-page headroom for `worker_count()` writers
        // (the parallel parse's writer count), so spawning more writers than that
        // can overflow it once enough of them grab a page. Match that count, with
        // a floor of two so this stays a concurrency test on a single core.
        let threads = worker_count().max(2);
        const DISTINCT: usize = 5000;
        let long = "z".repeat(1024);
        // A key each thread can reach, so the same strings recur across threads.
        let key = |i: usize| {
            if i.is_multiple_of(250) {
                format!("//very/long/package/path/number/{i}/{long}")
            } else {
                format!("//pkg{}/lib{}:target{}", i % 97, i % 13, i)
            }
        };
        let interner = ConcurrentInterner::new(DISTINCT * 64, DISTINCT).expect("a small pool");
        let produced: Vec<Vec<(String, Str)>> = bviz_parallel::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let interner = &interner;
                    let key = &key;
                    scope.spawn(move || {
                        let mut w = interner.writer();
                        let mut out = Vec::new();
                        // Overlapping strides so every thread sees a shared set.
                        for i in (t..DISTINCT).chain(0..DISTINCT).step_by(1) {
                            let s = key(i % DISTINCT);
                            let h = w.intern(&s);
                            out.push((s, h));
                        }
                        out
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let text = interner.into_text().expect("the pool holds its text");

        let mut by_text: HashMap<String, Str> = HashMap::new();
        let mut by_handle: HashMap<Str, String> = HashMap::new();
        for thread_out in &produced {
            for (s, h) in thread_out {
                assert_eq!(&text[h.range()], s, "handle round-trips to its text");
                if let Some(prev) = by_text.insert(s.clone(), *h) {
                    assert_eq!(prev, *h, "text {s:?} mapped to two handles");
                }
                if let Some(prev) = by_handle.insert(*h, s.clone()) {
                    assert_eq!(&prev, s, "handle shared by two distinct texts");
                }
            }
        }
        assert_eq!(by_text.len(), DISTINCT);
        assert_eq!(by_handle.len(), DISTINCT);
    }

    #[test]
    fn empty_string_gets_a_non_zero_handle() {
        // A fresh writer's first intern of "" must not land at offset 0 — that
        // is the placeholder and the OptStr absence encoding — and repeats of it
        // dedup to one handle that resolves to "".
        let interner = ConcurrentInterner::new(256, 4).expect("a small pool");
        let (empty, empty2, non_empty) = {
            let mut w = interner.writer();
            (w.intern(""), w.intern("x"), w.intern(""))
        };
        assert_eq!(empty, non_empty, "\"\" dedups to one handle");
        assert_ne!(empty2, empty);
        assert_ne!(empty.range().start, 0, "\"\" avoids the placeholder offset");
        let text = interner.into_text().expect("the pool holds its text");
        assert_eq!(&text[empty.range()], "");
        assert_eq!(&text[empty2.range()], "x");
    }

    #[test]
    fn strings_straddle_page_boundaries() {
        const PAGE: usize = 64;
        // Lengths chosen to exactly fill a page, straddle into the next, and
        // overflow repeatedly, crossing several boundaries.
        let seq = [
            "a".repeat(PAGE - 1), // fills the first page's [1, 64)
            "bb".to_string(),     // must start a fresh page
            "c".repeat(PAGE),     // exactly one full page
            "d".repeat(30),
            "e".repeat(40),       // 30 + 40 overflows a page mid-run
            "a".repeat(PAGE - 1), // a repeat of the first — must dedup
            "c".repeat(PAGE),     // a repeat crossing the boundary origin
        ];
        let interner = ConcurrentInterner::with_page_size(4096, seq.len(), PAGE);
        let handles: Vec<Str> = {
            let mut w = interner.writer();
            seq.iter().map(|s| w.intern(s)).collect()
        };
        let text = interner.into_text().expect("the pool holds its text");
        for (s, &h) in seq.iter().zip(&handles) {
            assert_eq!(&text[h.range()], s);
        }
        assert_eq!(handles[0], handles[5], "equal strings dedup across pages");
        assert_eq!(handles[2], handles[6]);
    }

    /// A string longer than a page takes a dedicated exact-size chunk. Runs in
    /// every build mode (the fit path is not a `debug_assert`), and the writer's
    /// current page must keep serving small strings around the oversized one.
    #[test]
    fn strings_larger_than_a_page_get_a_dedicated_chunk() {
        const PAGE: usize = 64;
        let over = "x".repeat(PAGE * 3 + 7);
        let seq = [
            "small-before".to_string(),
            over.clone(),
            "small-after".to_string(),
            over.clone(), // a repeat of the oversized string must dedup
        ];
        let interner = ConcurrentInterner::with_page_size(4096, seq.len(), PAGE);
        let handles: Vec<Str> = {
            let mut w = interner.writer();
            seq.iter().map(|s| w.intern(s)).collect()
        };
        let text = interner.into_text().expect("the pool holds its text");
        for (s, &h) in seq.iter().zip(&handles) {
            assert_eq!(&text[h.range()], s);
        }
        assert_eq!(handles[1], handles[3], "the oversized string dedups");
        assert_ne!(
            handles[0], handles[2],
            "small strings around it stay distinct"
        );
    }

    #[test]
    fn an_exhausted_pool_refuses_to_seal() {
        const PAGE: usize = 16;
        let interner = ConcurrentInterner::with_exact_pool(3 * PAGE, PAGE);
        {
            let mut w = interner.writer();
            for i in 0..64 {
                w.intern(&format!("string number {i}"));
            }
        }
        assert!(interner.exhausted());
        assert_eq!(interner.into_text(), Err(PoolOverflow::Exhausted));
    }

    #[test]
    fn a_pool_past_u32_offsets_is_refused() {
        assert_eq!(
            ConcurrentInterner::new(u32::MAX as usize, 0).err(),
            Some(PoolOverflow::TooLarge)
        );
    }
}
