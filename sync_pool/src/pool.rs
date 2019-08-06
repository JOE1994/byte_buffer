use std::mem::{self, MaybeUninit};
use std::ptr;
use std::sync::atomic::{self, AtomicBool, AtomicUsize, Ordering, AtomicPtr};

const POOL_SIZE: usize = 8;
const SLOT_CAP: usize = 32;
const EXPANSION_CAP: usize = 512;

/// Configuration flags
const CONFIG_ALLOW_EXPANSION: usize = 1;

type ResetHandle<T> = fn(&mut T);

struct Slot<T> {
    /// the actual data store
    slot: [Option<T>; SLOT_CAP],

    /// the current ready-to-use slot index, always offset by 1 to the actual index
    len: usize,

    /// if the slot is currently being read/write to
    access: AtomicBool,
}

//TODO: v2 -> only save/gave the pointer, and mem::forget the original value.

impl<T: Default> Slot<T> {
    fn new(fill: bool) -> Self {
        // create the placeholder
        let mut slice: [Option<T>; SLOT_CAP] = unsafe { MaybeUninit::zeroed().assume_init() };

        // fill the placeholder if required
        if fill {
            for i in 0..slice.len() {
//                let val = Default::default();
//                slice[i] = &val as *mut T;
//                mem::forget(val);
//
                slice[i] = Default::default();
            }
        }

        // done
        Slot {
            slot: slice,
            len: SLOT_CAP,
            access: AtomicBool::new(false),
        }
    }

    fn try_lock(&self, is_get: bool) -> bool {
        // be more patient if we're to return a value
        let mut count = if is_get { 4 } else { 6 };

        // check the access and wait if not available
        while self
            .access
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Acquire)
            .is_err()
        {
            cpu_relax(count);
            count -= 1;

            // "timeout" -- tried 4 times and still can't get the try_lock, rare case but fine, move on.
            if count == 0 {
                return false;
            }
        }

        if (is_get && self.len == 0) || (!is_get && self.len == SLOT_CAP) {
            // not actually locked
            self.unlock();

            // read but empty, or write but full, all fail
            return false;
        }

        true
    }

    fn unlock(&self) {
        self.access.store(false, Ordering::Release);
    }

    /// The function is safe because it's used internally, and each time it's guaranteed a try_lock has
    /// been acquired previously
    fn checkout(&mut self) -> Result<T, ()> {
        // need to loop over the slots to make sure we're getting the valid value, starting from
        let i = self.len - 1;
        if let Some(val) = self.slot[i].take() {
            // update internal states
            self.len = i;

            // return the value
            return Ok(val);
        }

        Err(())
    }

    /// The function is safe because it's used internally, and each time it's guaranteed a try_lock has
    /// been acquired previously
    fn release(&mut self, mut val: T, reset: *mut ResetHandle<T>) {
        // need to loop over the slots to make sure we're getting the valid value
        let i = self.len;
        if self.slot[i].is_none() {
            // reset the struct before releasing it to the pool
            if !reset.is_null() {
                unsafe { (*reset)(&mut val); }
            }

            // update internal states
            self.slot[i].replace(val);
            self.len = i;

            // done
            return;
        }

        // if all slots are full, no need to fallback, the `val` will be dropped here
        drop(val);
    }
}

struct VisitorGuard<'a>(&'a AtomicUsize);

impl<'a> VisitorGuard<'a> {
    fn register(base: &'a (AtomicUsize, AtomicBool)) -> Self {
        let mut count = 0;

        // wait if the underlying storage is in protection mode
        while base.1.load(Ordering::Acquire) {
            cpu_relax(count + 8);

            if count < 8 {
                count += 1;
            }
        }

        base.0.fetch_add(1, Ordering::SeqCst);
        VisitorGuard(&base.0)
    }
}

impl<'a> Drop for VisitorGuard<'a> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct SyncPool<T> {
    /// The slots storage
    slots: Vec<Slot<T>>,

    /// the next channel to try
    curr: AtomicUsize,

    /// First node: how many threads are concurrently accessing the struct:
    ///   0   -> updating the `slots` field;
    ///   1   -> no one is using the pool;
    ///   num -> number of visitors
    /// Second node: write barrier:
    ///   true  -> write barrier raised
    ///   false -> no write barrier
    visitor_counter: (AtomicUsize, AtomicBool),

    /// the number of times we failed to find an in-store struct to offer
    fault_count: AtomicUsize,

    /// if we allow expansion of the pool
    configure: AtomicUsize,

    /// the handle to be invoked before putting the struct back
    reset_handle: AtomicPtr<ResetHandle<T>>,
}

impl<T: Default> SyncPool<T> {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn with_size(size: usize) -> Self {
        let mut pool_size = size / SLOT_CAP;
        if pool_size < 1 {
            pool_size = 1
        }

        Self::make_pool(pool_size)
    }

    pub fn get(&mut self) -> T {
        // update user count
        let _guard = VisitorGuard::register(&self.visitor_counter);

        // start from where we're left
        let cap = self.slots.len();
        let origin: usize = self.curr.load(Ordering::Acquire) % cap;
        let mut pos = origin;

        loop {
            // check this slot
            let slot: &mut Slot<T> = &mut self.slots[pos];
            let next = if pos == cap - 1 { 0 } else { pos + 1 };

            // try the try_lock or move on
            if !slot.try_lock(true) {
                pos = next;

                // we've finished 1 loop but not finding a value to extract, quit
                if pos == origin {
                    break;
                }

                continue;
            }

            // try to checkout one slot
            let checkout = slot.checkout();
            slot.unlock();

            if let Ok(val) = checkout {
                // now we're locked, get the val and update internal states
                self.curr.store(next, Ordering::Release);

                // done
                return val;
            }

            // failed to checkout, break and let the remainder logic to handle the rest
            break;
        }

        // make sure our guard has been returned if we want the correct visitor count
        drop(_guard);

        Default::default()
    }

    pub fn put(&mut self, val: T) {
        // update user count
        let _guard = VisitorGuard::register(&self.visitor_counter);

        // start from where we're left
        let cap = self.slots.len();
        let curr: usize = self.curr.load(Ordering::Acquire) % cap;

        // origin is 1 `Slots` off from the next "get" position
        let origin = if curr > 0 { curr - 1 } else { 0 };

        let mut pos = origin;

        loop {
            // check this slot
            let slot: &mut Slot<T> = &mut self.slots[pos];
            let next = if pos == 0 { cap - 1 } else { pos - 1 };

            // try the try_lock or move on
            if !slot.try_lock(false) {
                pos = next;

                // we've finished 1 loop but not finding a value to extract, quit
                if pos == origin {
                    break;
                }

                continue;
            }

            // now we're locked, get the val and update internal states
            self.curr.store(pos, Ordering::Release);
            slot.release(val, self.reset_handle.load(Ordering::Acquire));
            slot.unlock();

            return;
        }
    }

    fn make_pool(size: usize) -> Self {
        let mut s = Vec::with_capacity(size);

        (0..size).for_each(|_| {
            // add the slice back to the vec container
            s.push(Slot::new(true));
        });

        SyncPool {
            slots: s,
            curr: AtomicUsize::new(0),
            visitor_counter: (AtomicUsize::new(1), AtomicBool::new(false)),
            fault_count: AtomicUsize::new(0),
            configure: AtomicUsize::new(0),
            reset_handle: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn update_config(&mut self, mask: usize, target: bool) {
        let mut curr = self.configure.load(Ordering::SeqCst);

        while let Err(old) = self
            .configure
            .compare_exchange(curr, curr ^ mask, Ordering::SeqCst, Ordering::Relaxed)
        {
            if !((old & mask > 0) ^ target) {
                // the configure already matches, we're done
                return;
            }

            curr = old;
        }
    }
}

impl<T> Default for SyncPool<T> where T: Default {
    fn default() -> Self {
        SyncPool::make_pool(POOL_SIZE)
    }
}

impl<T> Drop for SyncPool<T> {
    fn drop(&mut self) {
        self.slots.clear();

        unsafe {
            // now drop the reset handle if it's not null
            Box::from_raw(
                self.reset_handle.swap(ptr::null_mut(), Ordering::SeqCst)
            );
        }
    }
}

pub trait PoolState {
    fn expansion_enabled(&self) -> bool;
    fn fault_count(&self) -> usize;
}

impl<T> PoolState for SyncPool<T> {
    fn expansion_enabled(&self) -> bool {
        let configure = self.configure.load(Ordering::SeqCst);
        configure & CONFIG_ALLOW_EXPANSION > 0
    }

    fn fault_count(&self) -> usize {
        self.fault_count.load(Ordering::Acquire)
    }
}

pub trait PoolManager<T> {
    fn allow_expansion(&mut self, allow: bool);
    fn expand(&mut self, additional: usize, block: bool) -> bool;
    fn reset_handle(&mut self, handle: ResetHandle<T>);
}

impl<T> PoolManager<T> for SyncPool<T> where T: Default {
    fn allow_expansion(&mut self, allow: bool) {
        if !(self.expansion_enabled() ^ allow) {
            // not flipping the configuration, return
            return;
        }

        self.update_config(CONFIG_ALLOW_EXPANSION, allow);
    }

    fn expand(&mut self, additional: usize, block: bool) -> bool {
        // if the pool isn't allowed to expand, just return
        if !self.expansion_enabled() {
            return false;
        }

        // if exceeding the upper limit, quit
        if self.slots.len() > EXPANSION_CAP {
            return false;
        }

        // raise the write barrier now, if someone has already raised the flag to indicate the
        // intention to write, let me go away.
        if self
            .visitor_counter
            .1
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Acquire)
            .is_err()
        {
            return false;
        }

        // busy waiting ... for all visitors to leave
        let mut count: usize = 0;
        let safe =
            loop {
                match self
                    .visitor_counter
                    .0
                    .compare_exchange(1, 0, Ordering::SeqCst, Ordering::Relaxed)
                {
                    Ok(_) => break true,
                    Err(_) => {
                        cpu_relax(2);
                        count += 1;

                        if count > 8 && !block {
                            break false;
                        }
                    }
                }
            };

        if safe {
            // update the slots by pushing `additional` slots
            (0..additional).for_each(|_| {
                self.slots.push(Slot::new(true));
            });

            self.fault_count.store(0, Ordering::Release);
        }

        // update the internal states
        self.visitor_counter.0.store(1, Ordering::SeqCst);
        self.visitor_counter.1.store(false, Ordering::Release);

        safe
    }

    fn reset_handle(&mut self, handle: ResetHandle<T>) {
        let h = Box::new(handle);
        self.reset_handle.swap(Box::into_raw(h) as *mut ResetHandle<T>, Ordering::Release);
    }
}

#[inline(always)]
fn cpu_relax(count: usize) {
    for _ in 0..(1 << count) {
        atomic::spin_loop_hint()
    }
}