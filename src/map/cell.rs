use std::ptr;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32};
use std::sync::{Condvar, Mutex};

pub struct Cell<K, V> {
    link: Option<Box<EntryLink<K, V>>>,
    metadata: AtomicU32,
    wait_queue: AtomicPtr<WaitQueueEntry>,
    partial_hash_array: [u32; 10],
}

/// CellLocker
pub struct CellLocker<'a, K, V> {
    cell: &'a Cell<K, V>,
    metadata: u32,
}

struct EntryLink<K, V> {
    key_value_pair: (K, V),
    next: Option<Box<EntryLink<K, V>>>,
}

struct WaitQueueEntry {
    mutex: Mutex<bool>,
    condvar: Condvar,
    completed: AtomicBool,
    next: *mut WaitQueueEntry,
}

impl<K, V> Cell<K, V> {
    const LOCK_MASK: u32 = (!(0 as u32)) << 8;
    const XLOCK: u32 = 1 << 31;
    const SLOCK_MAX: u32 = Self::LOCK_MASK & (!Self::XLOCK);
    const SLOCK: u32 = 1 << 8;
    const SIZE_MASK: u32 = 1 << 8 - 1;
    const SIZE_MAX: u32 = Self::SIZE_MASK;
}

impl<K, V> Default for Cell<K, V> {
    fn default() -> Self {
        Cell {
            link: None,
            metadata: AtomicU32::new(0),
            wait_queue: AtomicPtr::new(ptr::null_mut()),
            partial_hash_array: [0; 10],
        }
    }
}

impl<'a, K, V> CellLocker<'a, K, V> {
    /// Creates a new CellLocker instance with the cell exclusively locked.
    fn lock_exclusive(cell: &'a Cell<K, V>) -> CellLocker<'a, K, V> {
        loop {
            if let Some(result) = Self::try_lock_exclusive(cell) {
                return result;
            }
            if let Some(result) = Self::wait_exclusive(&cell) {
                return result;
            }
        }
    }

    /// Creates a new CellLocker instance if the cell is exclusively locked.
    fn try_lock_exclusive(cell: &'a Cell<K, V>) -> Option<CellLocker<'a, K, V>> {
        let mut current = cell.metadata.load(Relaxed);
        loop {
            match cell.metadata.compare_exchange(
                current & (!Cell::<K, V>::XLOCK),
                current | Cell::<K, V>::XLOCK,
                Acquire,
                Relaxed,
            ) {
                Ok(result) => {
                    return Some(CellLocker {
                        cell: cell,
                        metadata: result | Cell::<K, V>::XLOCK,
                    })
                }
                Err(result) => {
                    if result & Cell::<K, V>::XLOCK == Cell::<K, V>::XLOCK {
                        current = result;
                        return None;
                    }
                    current = result;
                }
            }
        }
    }

    fn wait_exclusive(cell: &'a Cell<K, V>) -> Option<CellLocker<'a, K, V>> {
        let mut barrier = WaitQueueEntry::new(cell.wait_queue.load(Relaxed));
        let barrier_ptr: *mut WaitQueueEntry = &mut barrier;

        // insert itself into the wait queue
        while let Err(result) =
            cell.wait_queue
                .compare_exchange(barrier.next, barrier_ptr, Release, Relaxed)
        {
            barrier.next = result;
        }

        // try-lock again once the barrier is inserted into the wait queue
        let locked = Self::try_lock_exclusive(cell);
        if locked.is_some() {
            Self::wakeup(cell);
        }
        barrier.wait();
        locked
    }

    fn wakeup(cell: &'a Cell<K, V>) {
        let mut barrier_ptr: *mut WaitQueueEntry = cell.wait_queue.load(Acquire);
        while let Err(result) =
            cell.wait_queue
                .compare_exchange(barrier_ptr, ptr::null_mut(), Acquire, Relaxed)
        {
            barrier_ptr = result;
            if barrier_ptr == ptr::null_mut() {
                return;
            }
        }

        while barrier_ptr != ptr::null_mut() {
            let next_ptr = unsafe { (*barrier_ptr).next };
            unsafe {
                (*barrier_ptr).signal();
            };
            barrier_ptr = next_ptr;
        }
    }
}

impl WaitQueueEntry {
    fn new(wait_queue: *mut WaitQueueEntry) -> WaitQueueEntry {
        WaitQueueEntry {
            mutex: Mutex::new(false),
            condvar: Condvar::new(),
            completed: AtomicBool::new(false),
            next: wait_queue,
        }
    }

    fn wait(&self) {
        let mut completed = self.mutex.lock().unwrap();
        while !*completed {
            completed = self.condvar.wait(completed).unwrap();
        }
        while !self.completed.load(Relaxed) {}
    }

    fn signal(&self) {
        let mut completed = self.mutex.lock().unwrap();
        *completed = true;
        self.condvar.notify_one();
        drop(completed);
        self.completed.store(true, Relaxed);
    }
}

impl<'a, K, V> Drop for CellLocker<'a, K, V> {
    fn drop(&mut self) {
        let mut current = self.metadata;
        loop {
            assert!(current & Cell::<K, V>::LOCK_MASK != 0);
            let new = if current & Cell::<K, V>::XLOCK == Cell::<K, V>::XLOCK {
                current & (!Cell::<K, V>::XLOCK)
            } else {
                current - Cell::<K, V>::SLOCK
            };
            match self
                .cell
                .metadata
                .compare_exchange(current, new, Release, Relaxed)
            {
                Ok(_) => break,
                Err(result) => current = result,
            }
        }
        Self::wakeup(self.cell);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn basic_assumptions() {
        assert_eq!(std::mem::size_of::<Cell<u64, bool>>(), 64)
    }

    #[test]
    fn basic_exclusive_locker() {
        let threads = 12;
        let barrier = Arc::new(Barrier::new(threads));
        let cell: Arc<Cell<bool, u8>> = Arc::new(Default::default());
        let mut thread_handles = Vec::with_capacity(threads);
        for tid in 0..threads {
            let barrier_copied = barrier.clone();
            let cell_copied = cell.clone();
            let thread_id = tid;
            thread_handles.push(thread::spawn(move || {
                barrier_copied.wait();
                for i in 0..4096 {
                    let locker = CellLocker::lock_exclusive(&*cell_copied);
                    if i % 256 == 255 {
                        println!("locked {}:{}", thread_id, i);
                    }
                    drop(locker);
                }
            }));
        }
        for handle in thread_handles {
            handle.join().unwrap();
        }
    }
}
