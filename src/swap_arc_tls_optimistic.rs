use std::marker::PhantomData;
use std::{mem, ptr, thread};
use std::borrow::Borrow;
use std::cell::{Cell, UnsafeCell};
use std::fmt::{Debug, Display, Formatter};
use std::intrinsics::{likely, unlikely};
use std::mem::{align_of, ManuallyDrop, MaybeUninit, transmute};
use std::num::Wrapping;
use std::ops::Deref;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;
use crossbeam_utils::{Backoff, CachePadded};
use thread_local::ThreadLocal;

#[derive(Copy, Clone, Debug)]
pub enum UpdateResult {
    Ok,
    AlreadyUpdating,
    NoUpdate,
}

const fn most_sig_set_bit(val: usize) -> Option<u32> {
    let mut i = 0;
    let mut ret = None;
    while i < usize::BITS {
        if val & (1 << i) != 0 {
            ret = Some(i);
        }
        i += 1;
    }
    ret
}

/// A `SwapArc` is a data structure that allows for an `Arc`
/// to be passed around and swapped out with other `Arc`s.
/// In order to achieve this, an internal reference count
/// scheme is used which allows for very quick, low overhead
/// reads in the common case (no update) and will sill be
/// decently fast when an update is performed, as updates
/// only consist of 3 atomic instructions. When a new
/// `Arc` is to be stored in the `SwapArc`, it first tries to
/// immediately update the current pointer (the one all readers will see)
/// (this is possible, if no other update is being performed and if there are no readers left)
/// if this fails, it will `push` the update so that it will
/// be performed by the last reader to finish reading.
/// A read consists of loading the current pointer and
/// performing a clone operation on the `Arc`, thus
/// readers are very short-lived and shouldn't block
/// updates for very long, although writer starvation
/// is possible in theory, it probably won't every be
/// observed in practice because of the short-lived
/// nature of readers.

/// This variant of `SwapArc` has wait-free reads (although
/// this is at the cost of additional atomic instructions
/// (at least 1 additional load - this will never be more than 1 load if there are no updates happening).
pub struct SwapArcIntermediateTLS<T, D: DataPtrConvert<T> = Arc<T>, const METADATA_BITS: u32 = 0> {
    curr_ref_cnt: AtomicUsize, // the last bit is the `update` bit
    ptr: AtomicPtr<T>,
    intermediate_ref_cnt: AtomicUsize, // the last bit is the `update` bit
    intermediate_ptr: AtomicPtr<T>,
    updated: AtomicPtr<T>,
    thread_local: ThreadLocal<CachePadded<LocalData<T, D, METADATA_BITS>>>,
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> SwapArcIntermediateTLS<T, D, METADATA_BITS> {

    const UPDATE: usize = 1 << (usize::BITS - 1);
    const OTHER_UPDATE: usize = 1 << (usize::BITS - 2);
    // const FORCE_UPDATE: usize = 1 << (usize::BITS - 3); // FIXME: do we actually need a separate flag? - we probably do
    // FIXME: implement force updating!

    pub fn new(val: D) -> Arc<Self> {
        let free_bits = most_sig_set_bit(align_of::<T>()).unwrap_or(0);
        if free_bits < METADATA_BITS { // FIXME: make sure this check is optimized away at compile time!
            let expected = 1 << METADATA_BITS;
            let found = 1 << free_bits;
            panic!("The alignment of T is insufficient, expected `{}`, but found `{}`", expected, found);
        }
        let val = ManuallyDrop::new(val);
        let virtual_ref = val.as_ptr();
        Arc::new(Self {
            curr_ref_cnt: Default::default(),
            ptr: AtomicPtr::new(virtual_ref.cast_mut()),
            intermediate_ref_cnt: Default::default(),
            intermediate_ptr: AtomicPtr::new(virtual_ref.cast_mut()/*null_mut()*/),
            updated: AtomicPtr::new(null_mut()),
            thread_local: ThreadLocal::new(),
        })
    }

    /// SAFETY: this is only safe to call if the caller increments the
    /// reference count of the "object" `val` points to.
    pub unsafe fn new_raw(val: *const T) -> Arc<Self> {
        let free_bits = most_sig_set_bit(align_of::<T>()).unwrap_or(0);
        if free_bits < METADATA_BITS { // FIXME: make sure this check is optimized away at compile time!
            let expected = 1 << METADATA_BITS;
            let found = 1 << free_bits;
            panic!("The alignment of T is insufficient, expected `{}`, but found `{}`", expected, found);
        }
        Arc::new(Self {
            curr_ref_cnt: Default::default(),
            ptr: AtomicPtr::new(val.cast_mut()),
            intermediate_ref_cnt: Default::default(),
            intermediate_ptr: AtomicPtr::new(val.cast_mut()/*null_mut()*/),
            updated: AtomicPtr::new(null_mut()),
            thread_local: ThreadLocal::new(),
        })
    }

    // #[inline(never)]
    pub fn load<'a>(self: &'a Arc<Self>) -> SwapArcIntermediateGuard<'a, T, D, METADATA_BITS> {
        let parent = self.thread_local.get_or(|| {
            let curr = self.load_internal();
            let curr_ptr = curr.fake_ref.as_ptr();
            // increase the reference count
            mem::forget(curr.fake_ref.clone());
            CachePadded::new(LocalData {
                // SAFETY: this is safe because we know that this will only ever be accessed
                // if there is a life reference to `self` present.
                parent: ManuallyDrop::new(unsafe { Arc::from_raw(Arc::as_ptr(self)) }),
                inner: LocalDataInner {
                    next_gen_cnt: Cell::new(2),
                    intermediate: LocalCounted::default(),
                    new: LocalCounted { gen_cnt: Cell::new(0), ptr: Cell::new(null()), ref_cnt: Cell::new(usize::MAX), _phantom_data: Default::default() },
                    curr: LocalCounted { gen_cnt: Cell::new(1), ptr: Cell::new(curr_ptr), ref_cnt: Cell::new(1), _phantom_data: Default::default() }
                },
            })
        });
        // SAFETY: This is safe because we know that we are the only thread that
        // is able to access the thread local data at this time and said data has to be initialized
        // and we also know, that the pointer has to be non-null
        let data = &parent.inner;

        if likely(data.new.ref_cnt.get() == 0) {
            let (ptr, gen_cnt) = if likely(Self::strip_metadata(parent.parent.ptr.load(Ordering::Acquire)) == data.curr.ptr.get()) {
                data.curr.ref_cnt.update(|x| x + 1);
                (data.curr.ptr.get(), data.curr.gen_cnt.get())
            } else {
                load_new_slow(self, data)
            };
            let fake_ref = ManuallyDrop::new(unsafe { D::from(ptr) });
            return SwapArcIntermediateGuard {
                parent,
                fake_ref,
                gen_cnt,
            };
        } else if unlikely(data.new.ref_cnt.get() == usize::MAX) {
            data.new.ref_cnt.set(0);
            let fake_ref = ManuallyDrop::new(unsafe { D::from(data.curr.ptr.get()) });
            return SwapArcIntermediateGuard {
                parent,
                fake_ref,
                gen_cnt: data.curr.gen_cnt.get(),
            };
        }

        #[cold]
        fn load_new_slow<'a, T, D: DataPtrConvert<T>, const META_DATA_BITS: u32>(this: &'a Arc<SwapArcIntermediateTLS<T, D, { META_DATA_BITS }>>, data: &'a LocalDataInner<T, D>) -> (*const T, usize) {
            let curr = this.load_internal();
            // increase the strong reference count
            mem::forget(curr.fake_ref.clone());
            let new_ptr = curr.fake_ref.as_ptr();

            let gen_cnt = if data.curr.ref_cnt.get() == 0 {
                // don't modify the gen_cnt
                data.curr.refill_unchecked(new_ptr);
                data.curr.gen_cnt.get()
            } else {
                // data.new.refill(data, new_ptr);
                if data.new.gen_cnt.get() != 0 {
                    data.new.refill_unchecked(new_ptr);
                } else {
                    let new = LocalCounted::new(data, new_ptr);
                    data.new.set(new);
                }
                data.new.gen_cnt.get()
            };
            (new_ptr, gen_cnt)
        }

        #[cold]
        fn load_slow<'a, T, D: DataPtrConvert<T>, const META_DATA_BITS: u32>(parent: &'a LocalData<T, D, META_DATA_BITS>, data: &LocalDataInner<T, D>) -> SwapArcIntermediateGuard<'a, T, D, META_DATA_BITS> {
            if data.curr.ref_cnt.get() == 0 {
                // we can do the `curr` update on load because we know that we have a strong reference
                // to the value stored inside `ptr` anyways, so it doesn't really matter when exactly we update `curr`
                // and this allows us to save many branches on drop
                data.curr.set(data.new.take());
                if data.intermediate.ref_cnt.get() != 0 {
                    data.new.set(unsafe { data.intermediate.take().make_drop() });
                    // increase the ref count of the new value
                    mem::forget(data.new.val().clone());
                    parent.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                    let fake_ref = ManuallyDrop::new(unsafe { D::from(data.new.ptr.get()) });
                    return SwapArcIntermediateGuard {
                        parent,
                        fake_ref,
                        gen_cnt: data.new.gen_cnt.get(),
                    };
                }
                // FIXME: what do we do here? `curr` was replaced by `new` and `new` is empty now, do we return now, or do we allow a potential update of `intermediate`?
            }
            if data.intermediate.ref_cnt.get() == 0 {
                let intermediate = SwapArcIntermediateTLS::<T, D, { META_DATA_BITS }>::strip_metadata(parent.parent.intermediate_ptr.load(Ordering::Acquire));
                // check if there is a new intermediate value and that the intermediate value has been verified to be usable
                let (ptr, gen_cnt) = if intermediate != data.new.ptr.get() {
                    let loaded = parent.parent.intermediate_ref_cnt.fetch_add(1, Ordering::SeqCst);
                    if loaded & SwapArcIntermediateTLS::<T, D, { META_DATA_BITS }>::UPDATE == 0 {
                        let loaded = SwapArcIntermediateTLS::<T, D, { META_DATA_BITS }>::strip_metadata(parent.parent.intermediate_ptr.load(Ordering::Acquire));
                        // data.intermediate.refill(data, loaded);
                        if data.intermediate.gen_cnt.get() != 0 {
                            data.intermediate.refill_unchecked(loaded);
                        } else {
                            let new = LocalCounted::new(data, loaded);
                            data.intermediate.set(new);
                        }
                        (loaded, data.intermediate.gen_cnt.get())
                    } else {
                        parent.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                        data.new.ref_cnt.update(|x| x + 1);
                        (data.new.ptr.get(), data.new.gen_cnt.get())
                    }
                } else {
                    data.new.ref_cnt.update(|x| x + 1);
                    (data.new.ptr.get(), data.new.gen_cnt.get())
                };
                let fake_ref = ManuallyDrop::new(unsafe { D::from(ptr) });
                return SwapArcIntermediateGuard {
                    parent,
                    fake_ref,
                    gen_cnt,
                };
            } else {
                data.intermediate.ref_cnt.update(|x| x + 1);
                let fake_ref = ManuallyDrop::new(unsafe { D::from(data.intermediate.ptr.get()) });
                return SwapArcIntermediateGuard {
                    parent: &parent,
                    fake_ref,
                    gen_cnt: data.intermediate.gen_cnt.get(),
                };
            }
        }

        load_slow(parent, data)
    }

    pub fn load_full(self: &Arc<Self>) -> D {
        self.load().as_ref().clone()
    }

    pub unsafe fn load_raw<'a>(self: &'a Arc<Self>) -> SwapArcIntermediatePtrGuard<'a, T, D, METADATA_BITS> {
        let guard = ManuallyDrop::new(self.load());
        let curr_meta = {
            Self::get_metadata(self.intermediate_ptr.load(Ordering::Acquire)) // FIXME: is this okay - even, when `intermediate` is getting updated?
        };
        SwapArcIntermediatePtrGuard {
            parent: guard.parent,
            // FIXME: use this once strict provenance got stabilized!
            // ptr: guard.fake_ref.as_ptr().map_addr(|x| x | curr_meta),
            ptr: (guard.fake_ref.as_ptr() as usize | curr_meta) as *const T,
            gen_cnt: guard.gen_cnt,
        }
    }

    fn load_internal<'a>(self: &'a Arc<Self>) -> SwapArcIntermediateInternalGuard<'a, T, D, METADATA_BITS> {
        let ref_cnt = self.curr_ref_cnt.fetch_add(1, Ordering::SeqCst);
        let (ptr, src) = if ref_cnt & Self::UPDATE != 0 {
            let intermediate_ref_cnt = self.intermediate_ref_cnt.fetch_add(1, Ordering::SeqCst);
            if intermediate_ref_cnt & Self::UPDATE != 0 {
                let ret = self.ptr.load(Ordering::Acquire);
                // release the redundant reference
                self.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                (ret, RefSource::Curr)
            } else {
                let ret = self.intermediate_ptr.load(Ordering::Acquire);
                // release the redundant reference
                self.curr_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                (ret, RefSource::Intermediate)
            }
        } else {
            (self.ptr.load(Ordering::Acquire), RefSource::Curr)
        };
        // create a fake reference to the Arc to ensure so that the borrow checker understands
        // that the reference returned from the guard will point to valid memory
        let fake_ref = ManuallyDrop::new(unsafe { D::from(Self::strip_metadata(ptr)) });
        SwapArcIntermediateInternalGuard {
            parent: self,
            fake_ref,
            ref_src: src,
        }
    }

    fn try_update_curr(&self) -> bool {
        let ret = match self.curr_ref_cnt.compare_exchange(Self::OTHER_UPDATE, Self::UPDATE, Ordering::SeqCst, Ordering::Relaxed) {
            Ok(_) => {
                // FIXME: can we somehow bypass intermediate if we have a new update upcoming - we probably can't because this would probably cause memory leaks and other funny things that we don't like
                let intermediate = self.intermediate_ptr.load(Ordering::SeqCst);
                // update the pointer
                let prev = self.ptr.load(Ordering::Acquire);
                if Self::strip_metadata(prev) != Self::strip_metadata(intermediate) {
                    self.ptr.store(intermediate, Ordering::Release);
                    // unset the update flag
                    self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                    // drop the `virtual reference` we hold to the Arc
                    unsafe { D::from(Self::strip_metadata(prev)); }
                } else {
                    // unset the update flag
                    self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                }
                // unset the `weak` update flag from the intermediate ref cnt
                self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::SeqCst);
                true
            }
            _ => false,
        };
        ret
    }

    fn try_update_intermediate(&self) {
        match self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::SeqCst, Ordering::Relaxed) {
            Ok(_) => {
                // take the update
                let update = self.updated.swap(null_mut(), Ordering::SeqCst);
                // check if we even have an update
                if !update.is_null() {
                    let metadata = Self::get_metadata(self.intermediate_ptr.load(Ordering::Acquire));
                    let update = Self::merge_ptr_and_metadata(update, metadata).cast_mut();
                    self.intermediate_ptr.store(update, Ordering::Release);
                    // unset the update flag
                    self.intermediate_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                    // try finishing the update up!
                    match self.curr_ref_cnt.compare_exchange(0, Self::UPDATE, Ordering::SeqCst, Ordering::Relaxed) {
                        Ok(_) => {
                            let prev = self.ptr.swap(update, Ordering::SeqCst);
                            // unset the update flag
                            self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                            // unset the `weak` update flag from the intermediate ref cnt
                            self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::SeqCst);
                            // drop the `virtual reference` we hold to the Arc
                            unsafe { D::from(Self::strip_metadata(prev)); }
                        }
                        Err(_) => {}
                    }
                } else {
                    // unset the update flags
                    self.intermediate_ref_cnt.fetch_and(!(Self::UPDATE | Self::OTHER_UPDATE), Ordering::SeqCst);
                }
            }
            Err(_) => {}
        }
    }

    pub fn update(&self, updated: D) {
        unsafe { self.update_raw(updated.as_ptr()); }
    }

    unsafe fn update_raw(&self, updated: *const T) {
        let updated = Self::strip_metadata(updated);
        let new = updated.cast_mut();
        let tmp = ManuallyDrop::new(D::from(new));
        // increase the ref count
        mem::forget(tmp.clone());
        let backoff = Backoff::new();
        loop {
            match self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    // clear out old updates to make sure our update won't be overwritten by them in the future
                    let old = self.updated.swap(null_mut(), Ordering::SeqCst);
                    let metadata = Self::get_metadata(self.intermediate_ptr.load(Ordering::Acquire));
                    let new = Self::merge_ptr_and_metadata(new, metadata).cast_mut();
                    self.intermediate_ptr.store(new, Ordering::Release);
                    // unset the update flag
                    self.intermediate_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                    if !old.is_null() {
                        // drop the `virtual reference` we hold to the Arc
                        D::from(old);
                    }
                    // try finishing the update up!
                    loop {
                        let mut curr = 0;
                        match self.curr_ref_cnt.compare_exchange(curr, Self::UPDATE, Ordering::SeqCst, Ordering::Relaxed) {
                            Ok(_) => {
                                let prev = self.ptr.swap(new, Ordering::SeqCst);
                                // unset the update flag
                                self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                                // unset the `weak` update flag from the intermediate ref cnt
                                self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::SeqCst);
                                // drop the `virtual reference` we hold to the Arc
                                D::from(Self::strip_metadata(prev));
                                break;
                            }
                            Err(_) => {
                                // signal that it's allowed to implicitly update this now
                                if self.curr_ref_cnt.fetch_or(Self::OTHER_UPDATE, Ordering::SeqCst) == 0 {
                                    curr = Self::OTHER_UPDATE;
                                    // retry update, as no implicit update can occur anymore
                                    continue;
                                }
                                break;
                            }
                        }
                    }
                    break;
                }
                Err(old) => {
                    if old & Self::UPDATE != 0 {
                        backoff.snooze();
                        // somebody else already updates the current ptr, so we wait until they finish their update
                        continue;
                    }
                    // push our update up, so it will be applied in the future
                    let old = self.updated.swap(new, Ordering::SeqCst); // FIXME: should we add some sort of update counter
                    // FIXME: to determine which update is the most recent?
                    if !old.is_null() {
                        // drop the `virtual reference` we hold to the Arc
                        D::from(old);
                    }
                    break;
                }
            }
        }
    }

    /*
    unsafe fn try_compare_exchange<const IGNORE_META: bool>(&self, old: *const T, new: D) -> Result<bool, D> {
        // FIXME: what should be compared against? `curr`? or is `update` to be taken into account as well?
        // FIXME: a good solution could be to compare against both `curr` and `intermediate` of the "main struct"(`ArcSwapIntermediateTLSLessFence`)
        // FIXME: this could lead to a problem tho because it doesn't seem very bulletproof to simply compare against 2 values that could point to two entirely different allocations
        // FIXME: if not done with careful consideration this could defeat the whole purpose of implementing compare_exchange for the data structure
        // FIXME: because this could lead to clients of this function to get unreliable feedback and weird updates to be performed which is the worst case scenario and should be avoided at all costs
        // FIXME: another solution could be to let the caller provide whether to check for the intermediate or curr value which is okay because then this
        // FIXME: compare_exchange function would check if the state the caller last saw is still up to date or not which would (probably) solve all problems
        // FIXME: we had before - the only problem that remains is that this will (probably) not work for thread locals because 1.
        // FIXME: their states are inconsistent across different threads and because something loaded as `RefSource::Curr` can become `RefSource::Intermediate`
        // FIXME: without notice - although if the first point could be neglected, the second one can be solved, the only problem remaining is that once
        // FIXME: we compared the thread local states, we have to perform a global update that is propagated down to the source of the compare
        // FIXME: if we do that we have replaced the global state source and have replaced the local one, the only problem remaining still
        // FIXME: is if another thread tries to perform a compare_exchange, but this could be solved by using compare_exchange on the global state source
        // FIXME: and acquiring the update lock. but if the update failed we have to somehow update the local state because otherwise we could
        // FIXME: run into an infinite loop (QUESTION: is this really true? because if we load the most recent value from local, we won't get an expired one -
        // FIXME: the only possible thing that i can think of right now is when intermediate is not null but even then intermediate should always contain
        // FIXME: the most recent value and there shouldn't be a more recent one in global state sources (except for `updated` - but only if that is to be
        // FIXME: considered a "global state source")
        // FIXME: also what is to be considered the "global state source"? is it `curr` or `updated` - or both in some weird way?
        // FIXME: if the local has some `intermediate` value then we can simply use said value for comparison (but only if the provided source is also `intermediate`)
        // FIXME: OR maybe even if not?
        if !self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
            return Err(new);
        }
        let intermediate = self.intermediate_ptr.load(Ordering::Acquire);
        let cmp_result = if IGNORE_META {
            Self::strip_metadata(intermediate) == old
        } else {
            intermediate.cast_const() == old
        };
        if !cmp_result {
            self.intermediate_ref_cnt.fetch_and(!(Self::UPDATE | Self::OTHER_UPDATE), Ordering::SeqCst);
            return Ok(false);
        }
        // forget `new` in order to create a `virtual reference`
        let new = ManuallyDrop::new(new);
        let new = new.as_ptr();
        // clear out old updates to make sure our update won't be overwritten by them in the future
        let old_update = self.updated.swap(null_mut(), Ordering::SeqCst);
        let metadata = Self::get_metadata(intermediate);
        let new = Self::merge_ptr_and_metadata(new, metadata).cast_mut();
        self.intermediate_ptr.store(new, Ordering::Release);
        // unset the update flag
        self.intermediate_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
        if !old_update.is_null() {
            // drop the `virtual reference` we hold to the Arc
            D::from(old_update);
        }
        match self.curr_ref_cnt.compare_exchange(0, Self::UPDATE, Ordering::SeqCst, Ordering::Relaxed) {
            Ok(_) => {
                let prev = self.ptr.swap(new, Ordering::SeqCst);
                // unset the update flag
                self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                // unset the `weak` update flag from the intermediate ref cnt
                self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::SeqCst);
                // drop the `virtual reference` we hold to the Arc
                D::from(Self::strip_metadata(prev));
            }
            Err(_) => {}
        }
        Ok(true)
    }*/

    // FIXME: this causes "deadlocks" if there are any other references alive
    unsafe fn try_compare_exchange_with_meta(&self, old: *const T, new: *const T) -> bool {
        let backoff = Backoff::new();
        while !self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
            // back-off
            backoff.snooze();
        }
        let intermediate = self.intermediate_ptr.load(Ordering::Acquire);
        if intermediate.cast_const() != old {
            self.intermediate_ref_cnt.fetch_and(!(Self::UPDATE | Self::OTHER_UPDATE), Ordering::SeqCst);
            return false;
        }
        // clear out old updates to make sure our update won't be overwritten by them in the future
        let old_update = self.updated.swap(null_mut(), Ordering::SeqCst);
        // increase the ref count
        let tmp = ManuallyDrop::new(D::from(Self::strip_metadata(new)));
        mem::forget(tmp.clone());
        self.intermediate_ptr.store(new.cast_mut(), Ordering::Release);
        // unset the update flag
        self.intermediate_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
        if !old_update.is_null() {
            // drop the `virtual reference` we hold to the Arc
            D::from(old_update);
        }
        loop {
            let mut curr = 0;
            match self.curr_ref_cnt.compare_exchange(curr, Self::UPDATE, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(_) => {
                    let prev = self.ptr.swap(new.cast_mut(), Ordering::SeqCst/*Ordering::Release*/);
                    // unset the update flag
                    self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                    // unset the `weak` update flag from the intermediate ref cnt
                    self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::SeqCst);
                    // drop the `virtual reference` we hold to the Arc
                    D::from(Self::strip_metadata(prev));
                    break;
                }
                Err(_) => {
                    // signal that it's allowed to implicitly update this now
                    if self.curr_ref_cnt.fetch_or(Self::OTHER_UPDATE, Ordering::SeqCst) == 0 {
                        curr = Self::OTHER_UPDATE;
                        // retry update, as no implicit update can occur anymore
                        continue;
                    }
                    break;
                }
            }
        }
        true
    }

    pub fn update_metadata(&self, metadata: usize) {
        let backoff = Backoff::new();
        loop {
            let curr = self.intermediate_ptr.load(Ordering::Acquire);
            if self.try_update_meta(curr, metadata) { // FIXME: should this be a weak compare_exchange?
                break;
            }
            backoff.spin(); // FIXME: should we really backoff here? the other thread will make progress anyways and we will only have to spin once more if it makes progress again
        }
    }

    /// `old` should contain the previous metadata.
    pub fn try_update_meta(&self, old: *const T, metadata: usize) -> bool {
        let prefix = metadata & Self::META_MASK;
        // FIXME: use this once strict provenance got stabilized
        // self.intermediate_ptr.compare_exchange(old.cast_mut(), old.map_addr(|x| (x & !Self::META_MASK) | prefix).cast_mut(), Ordering::SeqCst, Ordering::Relaxed).is_ok()
        self.intermediate_ptr.compare_exchange(old.cast_mut(), ((old as usize & !Self::META_MASK) | prefix) as *mut T, Ordering::SeqCst, Ordering::Relaxed).is_ok()
    }

    pub fn set_in_metadata(&self, active_bits: usize) {
        let backoff = Backoff::new();
        loop {
            let curr = self.intermediate_ptr.load(Ordering::Acquire);
            let prefix = active_bits & Self::META_MASK;
            // FIXME: use this once strict provenance got stabilized
            // if self.intermediate_ptr.compare_exchange(curr, curr.map_addr(|x| x | prefix), Ordering::SeqCst, Ordering::Relaxed).is_ok() { // FIXME: should this be a weak compare_exchange?
            if self.intermediate_ptr.compare_exchange(curr, (curr as usize | prefix) as *mut T, Ordering::SeqCst, Ordering::Relaxed).is_ok() { // FIXME: should this be a weak compare_exchange?
                break;
            }
            backoff.spin(); // FIXME: should we really backoff here? the other thread will make progress anyways and we will only have to spin once more if it makes progress again
        }
    }

    pub fn unset_in_metadata(&self, inactive_bits: usize) {
        let backoff = Backoff::new();
        loop {
            let curr = self.intermediate_ptr.load(Ordering::Acquire);
            let prefix = inactive_bits & Self::META_MASK;
            // FIXME: use this once strict provenance got stabilized!
            //if self.intermediate_ptr.compare_exchange(curr, curr.map_addr(|x| x & !prefix), Ordering::SeqCst, Ordering::Relaxed).is_ok() { // FIXME: should this be a weak compare_exchange?
            if self.intermediate_ptr.compare_exchange(curr, (curr as usize & !prefix) as *mut T, Ordering::SeqCst, Ordering::Relaxed).is_ok() { // FIXME: should this be a weak compare_exchange?
                break;
            }
            backoff.spin(); // FIXME: should we really backoff here? the other thread will make progress anyways and we will only have to spin once more if it makes progress again
        }
    }

    pub fn load_metadata(&self) -> usize {
        self.intermediate_ptr.load(Ordering::Acquire) as usize & Self::META_MASK
    }

    #[inline]
    fn get_metadata(ptr: *const T) -> usize {
        ptr as usize & Self::META_MASK
    }

    #[inline]
    fn strip_metadata(ptr: *const T) -> *const T {
        // FIXME: use this once strict_provenance has been stabilized
        // ptr.map_addr(|x| x & !Self::META_MASK)
        (ptr as usize & !Self::META_MASK) as *const T
    }

    #[inline]
    fn merge_ptr_and_metadata(ptr: *const T, metadata: usize) -> *const T {
        // FIXME: use this once strict_provenance has been stabilized
        // ptr.map_addr(|x| x | metadata)
        (ptr as usize | metadata) as *const T
    }

    const META_MASK: usize = {
        let mut result = 0;
        let mut i = 0;
        while METADATA_BITS > i {
            result |= 1 << i;
            i += 1;
        }
        result
    };

    /*
    /// This will force an update, this means that new
    /// readers will have to wait for all old readers to
    /// finish and to update the ptr, even when no update
    /// is queued this will block new readers for a short
    /// amount of time, until failure got detected
    fn force_update(&self) -> UpdateResult {
        let curr = self.ref_cnt.fetch_or(Self::FORCE_UPDATE, Ordering::SeqCst);
        if curr & Self::UPDATE != 0 {
            return UpdateResult::AlreadyUpdating;
        }
        if self.updated.load(Ordering::SeqCst).is_null() {
            // unset the flag, as there are no upcoming updates
            self.ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
            return UpdateResult::NoUpdate;
        }
        UpdateResult::Ok
    }*/

}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop for SwapArcIntermediateTLS<T, D, METADATA_BITS> {
    fn drop(&mut self) {
        // FIXME: how should we handle intermediate inside drop?
        let updated = *self.updated.get_mut();
        if !updated.is_null() {
            unsafe { D::from(updated); }
        }
        let curr = Self::strip_metadata(*self.ptr.get_mut());
        let intermediate = Self::strip_metadata(*self.intermediate_ptr.get_mut());
        if intermediate != curr {
            // FIXME: the reason why we have to do this currently is because the update function doesn't work properly, fix the root cause!
            unsafe { D::from(intermediate); }
        }
        // drop the current arc
        unsafe { D::from(curr); }
    }
}

pub struct SwapArcIntermediatePtrGuard<'a, T, D: DataPtrConvert<T> = Arc<T>, const METADATA_BITS: u32 = 0> {
    parent: &'a LocalData<T, D, METADATA_BITS>,
    ptr: *const T,
    gen_cnt: usize,
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> SwapArcIntermediatePtrGuard<'_, T, D, METADATA_BITS> {

    #[inline]
    pub fn as_raw(&self) -> *const T {
        self.ptr
    }

}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Clone for SwapArcIntermediatePtrGuard<'_, T, D, METADATA_BITS> {
    fn clone(&self) -> Self {
        unsafe { self.parent.parent.load_raw() }
    }
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop for SwapArcIntermediatePtrGuard<'_, T, D, METADATA_BITS> {
    fn drop(&mut self) {
        // SAFETY: This is safe because we know that we are the only thread that
        // is able to access the thread local data at this time and said data has to be initialized
        // and we also know, that the pointer has to be non-null
        let data = &self.parent.inner;
        // release the reference we hold
        if likely(self.gen_cnt == data.curr.gen_cnt.get()) {
            /*if data.curr.ref_cnt == 0 {
                println!("1: {} | 2: {} | 3: {} | 4: {}", self.fake_ref.as_ptr() == data.new.ptr, data.new.ref_cnt, self.fake_ref.as_ptr() == data.intermediate.ptr, data.intermediate.ref_cnt);
            }*/
            data.curr.ref_cnt.update(|x| x - 1);
            /*if data.curr.ref_cnt == 0 {
                    if data.new.ref_cnt != 0 {
                        data.curr = mem::take(&mut data.new);
                            if data.intermediate.ref_cnt != 0 {
                                data.new = mem::take(&mut data.intermediate).make_drop();
                                // increase the ref count of the new value
                                mem::forget(data.new.val().clone());
                                self.parent.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                            }/* else if !data.intermediate.ptr.is_null() {
                                // FIXME: add a fallback case for this!
                            }*/
                    }/* else if !data.new.ptr.is_null() {
                        // FIXME: add a fallback case for this!
                    }*/
            }*/
        } else {
            slow_drop(self.parent, data, self.gen_cnt);
        }
        #[cold]
        fn slow_drop<T, D: DataPtrConvert<T>, const METADATA_BITS: u32>(parent: &LocalData<T, D, METADATA_BITS>, data: &LocalDataInner<T, D>, gen_cnt: usize) {
            if gen_cnt == data.new.gen_cnt.get() {
                data.new.ref_cnt.update(|x| x - 1);
                if data.new.ref_cnt.get() == 0 {
                    if data.intermediate.ref_cnt.get() != 0 {
                        data.new.set(unsafe { data.intermediate.take().make_drop() });
                        // increase the ref count of the new value
                        mem::forget(data.new.val().clone());
                        parent.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            } else {
                data.intermediate.ref_cnt.update(|x| x - 1);
                if data.intermediate.ref_cnt.get() == 0 {
                    parent.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                }
            }
        }
    }
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Debug for SwapArcIntermediatePtrGuard<'_, T, D, METADATA_BITS> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let tmp = format!("{:?}", self.ptr);
        f.write_str(tmp.as_str())
    }
}


pub struct SwapArcIntermediateGuard<'a, T, D: DataPtrConvert<T> = Arc<T>, const METADATA_BITS: u32 = 0> {
    parent: &'a LocalData<T, D, METADATA_BITS>,
    fake_ref: ManuallyDrop<D>,
    gen_cnt: usize,
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop for SwapArcIntermediateGuard<'_, T, D, METADATA_BITS> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: This is safe because we know that we are the only thread that
        // is able to access the thread local data at this time and said data has to be initialized
        // and we also know, that the pointer has to be non-null
        let data = &self.parent.inner;
        // release the reference we hold
        if likely(self.gen_cnt == data.curr.gen_cnt.get()) {
            /*if data.curr.ref_cnt == 0 {
                println!("1: {} | 2: {} | 3: {} | 4: {}", self.fake_ref.as_ptr() == data.new.ptr, data.new.ref_cnt, self.fake_ref.as_ptr() == data.intermediate.ptr, data.intermediate.ref_cnt);
            }*/
            data.curr.ref_cnt.update(|x| x - 1);
            /*if data.curr.ref_cnt == 0 {
                    if data.new.ref_cnt != 0 {
                        data.curr = mem::take(&mut data.new);
                            if data.intermediate.ref_cnt != 0 {
                                data.new = mem::take(&mut data.intermediate).make_drop();
                                // increase the ref count of the new value
                                mem::forget(data.new.val().clone());
                                self.parent.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                            }/* else if !data.intermediate.ptr.is_null() {
                                // FIXME: add a fallback case for this!
                            }*/
                    }/* else if !data.new.ptr.is_null() {
                        // FIXME: add a fallback case for this!
                    }*/
            }*/
        } else {
            slow_drop(self.parent, data, self.gen_cnt);
        }
        #[cold]
        fn slow_drop<T, D: DataPtrConvert<T>, const METADATA_BITS: u32>(parent: &LocalData<T, D, METADATA_BITS>, data: &LocalDataInner<T, D>, gen_cnt: usize) {
            if gen_cnt == data.new.gen_cnt.get() {
                data.new.ref_cnt.update(|x| x - 1);
                if data.new.ref_cnt.get() == 0 {
                    if data.intermediate.ref_cnt.get() != 0 {
                        data.new.set(unsafe { data.intermediate.take().make_drop() });
                        // increase the ref count of the new value
                        mem::forget(data.new.val().clone());
                        parent.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            } else {
                data.intermediate.ref_cnt.update(|x| x - 1);
                if data.intermediate.ref_cnt.get() == 0 {
                    parent.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                }
            }
        }
    }
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Deref for SwapArcIntermediateGuard<'_, T, D, METADATA_BITS> {
    type Target = D;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.fake_ref.deref()
    }
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Borrow<D> for SwapArcIntermediateGuard<'_, T, D, METADATA_BITS> {
    #[inline]
    fn borrow(&self) -> &D {
        self.fake_ref.deref()
    }
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> AsRef<D> for SwapArcIntermediateGuard<'_, T, D, METADATA_BITS> {
    #[inline]
    fn as_ref(&self) -> &D {
        self.fake_ref.deref()
    }
}

impl<T, D: DataPtrConvert<T> + Display, const METADATA_BITS: u32> Display for SwapArcIntermediateGuard<'_, T, D, METADATA_BITS> {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        D::fmt(self.as_ref(), f)
    }
}

impl<T, D: DataPtrConvert<T> + Debug, const METADATA_BITS: u32> Debug for SwapArcIntermediateGuard<'_, T, D, METADATA_BITS> {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        D::fmt(self.as_ref(), f)
    }
}


struct SwapArcIntermediateInternalGuard<'a, T, D: DataPtrConvert<T> = Arc<T>, const METADATA_BITS: u32 = 0> {
    parent: &'a Arc<SwapArcIntermediateTLS<T, D, METADATA_BITS>>,
    fake_ref: ManuallyDrop<D>,
    ref_src: RefSource,
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop for SwapArcIntermediateInternalGuard<'_, T, D, METADATA_BITS> {
    fn drop(&mut self) {
        // release the reference we hold
        match self.ref_src {
            RefSource::Curr => {
                let ref_cnt = self.parent.curr_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                if ref_cnt == SwapArcIntermediateTLS::<T, D, METADATA_BITS>::OTHER_UPDATE + 1/*1*/ { // FIXME: is it okay to do nothing on 1?
                    self.parent.try_update_curr();
                }
            }
            RefSource::Intermediate => {
                let ref_cnt = self.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                // fast-rejection path to ensure we are only trying to update if it's worth it
                // FIXME: this probably isn't correct: Note: UPDATE is set (seldom) on the immediate ref_cnt if there is a forced update waiting in the queue - this comment is pretty outdated as at this point in time forced updating isn't really relevant at all anymore.
                if (ref_cnt == 1/* || ref_cnt == SwapArcIntermediate::<T>::UPDATE*/) && !self.parent.updated.load(Ordering::Acquire).is_null() {
                    self.parent.try_update_intermediate();
                }
            }
        }
    }
}

impl<T, D: DataPtrConvert<T> + Display, const METADATA_BITS: u32> Display for SwapArcIntermediateInternalGuard<'_, T, D, METADATA_BITS> {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        D::fmt(self.fake_ref.deref(), f)
    }
}

impl<T, D: DataPtrConvert<T> + Debug, const METADATA_BITS: u32> Debug for SwapArcIntermediateInternalGuard<'_, T, D, METADATA_BITS> {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        D::fmt(self.fake_ref.deref(), f)
    }
}

enum RefSource {
    Curr,
    Intermediate,
}

struct LocalData<T, D: DataPtrConvert<T> = Arc<T>, const METADATA_BITS: u32 = 0> {
    parent: ManuallyDrop<Arc<SwapArcIntermediateTLS<T, D, METADATA_BITS>>>, // this acts as a reference with hidden lifetime that only we know is safe because
                                                                                   // `parent` won't be used in the drop impl and `LocalData` can only be accessed
                                                                                   // through `parent`
    inner: LocalDataInner<T, D>,
}

// FIXME: add safety comment (the gist is that this will only ever be used when `parent` is dropped)
unsafe impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Sync for LocalData<T, D, METADATA_BITS> {}

struct LocalDataInner<T, D: DataPtrConvert<T> = Arc<T>> {
    next_gen_cnt: Cell<usize>,
    intermediate: LocalCounted<T, D>,
    new: LocalCounted<T, D, true>,
    curr: LocalCounted<T, D, true>,
}

struct LocalCounted<T, D: DataPtrConvert<T> = Arc<T>, const DROP: bool = false> {
    gen_cnt: Cell<usize>, // FIXME: would it help somehow (the performance) if we were to make `gen_cnt` an `u8`?
    ptr: Cell<*const T>,
    ref_cnt: Cell<usize>,
    _phantom_data: PhantomData<D>,
}

impl<T, D: DataPtrConvert<T>, const DROP: bool> LocalCounted<T, D, DROP> {

    fn new(parent: &LocalDataInner<T, D>, ptr: *const T) -> Self {
        let gen_cnt = parent.next_gen_cnt.get();
        let res = gen_cnt.overflowing_add(1);
        // if an overflow occurs, we add an additional 1 to the result in order to never
        // reach 0
        parent.next_gen_cnt.set(res.0 + unsafe { transmute::<bool, u8>(res.1) } as usize);
        Self {
            gen_cnt: Cell::new(gen_cnt),
            ptr: Cell::new(ptr),
            ref_cnt: Cell::new(1),
            _phantom_data: Default::default(),
        }
    }

    #[inline]
    fn val(&self) -> ManuallyDrop<D> {
        ManuallyDrop::new(unsafe { D::from(self.ptr.get()) })
    }

    /*
    fn refill(&mut self, data: &LocalDataInner<T, D>, ptr: *const T) {
        if self.gen_cnt != 0 {
            self.refill_unchecked(ptr);
        } else {
            let new = LocalCounted::new(data, ptr);
            drop(mem::replace(self, new));
        }
    }*/

    fn refill_unchecked(&self, ptr: *const T) {
        if DROP {
            if !self.ptr.get().is_null() {
                // SAFETY: the person defining this struct has to make sure that
                // choosing `DROP` is correct.
                unsafe { D::from(self.ptr.get()); }
            }
        }
        self.ptr.set(ptr);
        self.ref_cnt.set(1);
    }

    fn take(&self) -> Self {
        Self {
            gen_cnt: Cell::new(self.gen_cnt.take()),
            ptr: Cell::new(self.ptr.replace(null())),
            ref_cnt: Cell::new(self.ref_cnt.take()),
            _phantom_data: Default::default(),
        }
    }

    fn set(&self, new: Self) {
        if DROP && !self.ptr.get().is_null() {
            unsafe { D::from(self.ptr.get()); }
        }
        self.ptr.set(new.ptr.get());
        self.ref_cnt.set(new.ref_cnt.get());
        self.gen_cnt.set(new.gen_cnt.get());
        mem::forget(new);
    }

}

impl<T, D: DataPtrConvert<T>> LocalCounted<T, D, false> {

    #[inline]
    unsafe fn make_drop(mut self) -> LocalCounted<T, D, true> {
        LocalCounted {
            gen_cnt: Cell::new(self.gen_cnt.get()),
            ptr: Cell::new(self.ptr.get()),
            ref_cnt: Cell::new(self.ref_cnt.get()),
            _phantom_data: Default::default(),
        }
    }

}

impl<T, D: DataPtrConvert<T>, const DROP: bool> Default for LocalCounted<T, D, DROP> {
    #[inline]
    fn default() -> Self {
        Self {
            gen_cnt: Cell::new(0),
            ptr: Cell::new(null()),
            ref_cnt: Cell::new(0),
            _phantom_data: Default::default(),
        }
    }
}

// FIXME: is `Send` safe to implement even for `DROP = true`? - it probably is!
// FIXME: add safety comment (the gist is that this will only ever be used when `parent` is dropped)
unsafe impl<T, D: DataPtrConvert<T>, const DROP: bool> Send for LocalCounted<T, D, DROP> {}

impl<T, D: DataPtrConvert<T>, const DROP: bool> Drop for LocalCounted<T, D, DROP> {
    #[inline]
    fn drop(&mut self) {
        if DROP {
            if !self.ptr.get().is_null() {
                // SAFETY: the person defining this struct has to make sure that
                // choosing `DROP` is correct.
                unsafe { D::from(self.ptr.get()); }
            }
        }
    }
}

/// SAFETY: Types implementing this trait are expected to perform
/// reference counting through cloning/dropping internally.
pub unsafe trait RefCnt: /*Send + Sync + */Clone {} // FIXME: is not having a Send + Sync bound correct here?

pub trait DataPtrConvert<T>: RefCnt + Sized {

    /// This function may not alter the reference count of the
    /// reference counted "object".
    unsafe fn from(ptr: *const T) -> Self;

    /// This function should decrement the reference count of the
    /// reference counted "object" indirectly, by automatically
    /// decrementing it on drop inside the "object"'s drop
    /// implementation.
    fn into(self) -> *const T;

    /// This function should NOT decrement the reference count of the
    /// reference counted "object" in any way, shape or form.
    fn as_ptr(&self) -> *const T;

    /// This function should increment the reference count of the
    /// reference counted "object" directly.
    #[inline]
    fn increase_ref_cnt(&self) {
        mem::forget(self.clone());
    }

}

unsafe impl<T> RefCnt for Arc<T> {}

impl<T> DataPtrConvert<T> for Arc<T> {

    #[inline]
    unsafe fn from(ptr: *const T) -> Self {
        Arc::from_raw(ptr)
    }

    fn into(self) -> *const T {
        let ret = Arc::into_raw(self);
        // decrement the reference count
        unsafe { <Self as DataPtrConvert<T>>::from(ret); }
        ret
    }

    #[inline]
    fn as_ptr(&self) -> *const T {
        Arc::as_ptr(self)
    }
}

unsafe impl<T> RefCnt for Option<Arc<T>> {}

impl<T> DataPtrConvert<T> for Option<Arc<T>> {

    unsafe fn from(ptr: *const T) -> Self {
        if !ptr.is_null() {
            Some(Arc::from_raw(ptr))
        } else {
            None
        }
    }

    fn into(self) -> *const T {
        match self {
            None => null(),
            Some(val) => {
                let ret = Arc::into_raw(val);
                // decrement the reference count
                unsafe { <Self as DataPtrConvert<T>>::from(ret); }
                ret
            },
        }
    }

    fn as_ptr(&self) -> *const T {
        match self {
            None => null(),
            Some(val) => Arc::as_ptr(val),
        }
    }
}
