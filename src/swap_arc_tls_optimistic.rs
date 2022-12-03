use std::marker::PhantomData;
use std::mem;
use std::borrow::Borrow;
use std::cell::{RefCell, UnsafeCell};
use std::fmt::{Debug, Display, Formatter};
use std::intrinsics::{likely, unlikely};
use std::mem::{align_of, ManuallyDrop};
use std::ops::Deref;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicUsize, fence, Ordering};
use cfg_if::cfg_if;
use crossbeam_utils::{Backoff, CachePadded};
use thread_local::ThreadLocal;

/// A `SwapArc` is a data structure that allows for an `Arc`
/// to be passed around and swapped out with other `Arc`s.
/// In order to achieve this, an internal reference count
/// scheme is used which allows for very quick, low overhead
/// reads in the common case (no update) and will sill be
/// decently fast when an update is performed. When a new
/// `Arc` is to be stored in the `SwapArc`, it first tries to
/// immediately update the current pointer (the one all readers will see in the uncontended case)
/// (this is possible, if no other update is being performed and if there are no readers left)
/// if this fails, it will try to do the same thing with the `intermediate` pointer
/// and if even that fails, it will `push` the update so that it will
/// be performed by the last `intermediate` reader to finish reading.
/// A read consists of loading the current pointer and
/// performing a clone operation on the `Arc`, thus
/// readers are very short-lived and can't block
/// updates for very long.
/// A `read` in this case refers to the process of acquiring
/// the shared pointer which gets handled by the `SwapArc`
/// itself internally. Note that such a `read` isn't what would
/// typically happen when `load` gets called as such a call
/// usually (in the case of no update) will only result
/// in a single `Relaxed` atomic load and a couple of
/// non-atomic bookkeeping operations utilizing TLS
/// to cache atomic accesses.

/// This variant of `SwapArc` has wait-free reads.
///
pub struct SwapArcIntermediateTLS<T, D: DataPtrConvert<T> = Arc<T>, const METADATA_BITS: u32 = 0> {
    curr_ref_cnt: AtomicUsize, // the last bit is the `update` bit
    ptr: AtomicPtr<T>,
    intermediate_ref_cnt: AtomicUsize, // the last bit is the `update` bit
    intermediate_ptr: AtomicPtr<T>,
    updated: AtomicPtr<T>,
    thread_local: ThreadLocal<CachePadded<LocalData<T, D, METADATA_BITS>>>,
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

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> SwapArcIntermediateTLS<T, D, METADATA_BITS> {

    const UPDATE: usize = 1 << (usize::BITS - 1);
    const OTHER_UPDATE: usize = 1 << (usize::BITS - 2);

    /// Creates a new `SwapArc` instance with `val` as the initial value.
    pub fn new(val: D) -> Self {
        let free_bits = most_sig_set_bit(align_of::<T>()).unwrap_or(0);
        if free_bits < METADATA_BITS {
            let expected = 1 << METADATA_BITS;
            let found = 1 << free_bits;
            panic!("The alignment of T is insufficient, expected `{}`, but found `{}`", expected, found);
        }
        let val = ManuallyDrop::new(val);
        let virtual_ref = val.as_ptr();
        Self {
            curr_ref_cnt: Default::default(),
            ptr: AtomicPtr::new(virtual_ref.cast_mut()),
            intermediate_ref_cnt: Default::default(),
            intermediate_ptr: AtomicPtr::new(virtual_ref.cast_mut()),
            updated: AtomicPtr::new(null_mut()),
            thread_local: ThreadLocal::new(),
        }
    }

    /// SAFETY: this is only safe to call if the caller increments the
    /// reference count of the "object" `val` points to.
    #[cfg(feature = "ptr-ops")]
    pub unsafe fn new_raw(val: *const T) -> Self {
        let free_bits = most_sig_set_bit(align_of::<T>()).unwrap_or(0);
        if free_bits < METADATA_BITS {
            let expected = 1 << METADATA_BITS;
            let found = 1 << free_bits;
            panic!("The alignment of T is insufficient, expected `{}`, but found `{}`", expected, found);
        }
        Self {
            curr_ref_cnt: Default::default(),
            ptr: AtomicPtr::new(val.cast_mut()),
            intermediate_ref_cnt: Default::default(),
            intermediate_ptr: AtomicPtr::new(val.cast_mut()),
            updated: AtomicPtr::new(null_mut()),
            thread_local: ThreadLocal::new(),
        }
    }

    /// Loads the reference counted stored value partially.
    pub fn load<'a>(&'a self) -> SwapArcIntermediateGuard<'a, T, D, METADATA_BITS> {
        let parent = self.thread_local.get_or(|| {
            let curr = self.load_internal();
            let curr_ptr = curr.fake_ref.as_ptr();
            // increase the reference count
            mem::forget(curr.fake_ref.clone());
            CachePadded::new(LocalData {
                // SAFETY: this is safe because we know that this will only ever be accessed
                // if there is a life reference to `self` present.
                parent: self as *const Self,
                inner: UnsafeCell::new(LocalDataInner {
                    next_gen_cnt: 2,
                    intermediate: LocalCounted::default(),
                    new: LocalCounted { gen_cnt: 0, ptr: null(), ref_cnt: usize::MAX, _phantom_data: Default::default() },
                    curr: LocalCounted { gen_cnt: 1, ptr: curr_ptr, ref_cnt: 1, _phantom_data: Default::default() }
                }),
            })
        });
        // SAFETY: This is safe because we know that we are the only thread that
        // is able to access the thread local data at this time and said data has to be initialized
        // and we also know, that the pointer has to be non-null
        let data = unsafe { parent.inner.get().as_mut().unwrap_unchecked() };

        // ORDERING: `ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        let no_update = Self::strip_metadata(parent.parent().ptr.load(Ordering::Relaxed)) == data.curr.ptr;
        if likely(no_update && data.new.ref_cnt == 0) {
            data.curr.ref_cnt += 1;
            // SAFETY: this is safe because the pointer contained inside `curr.ptr` is guaranteed to always be valid
            let fake_ref = ManuallyDrop::new(unsafe { D::from(data.curr.ptr) });
            return SwapArcIntermediateGuard {
                parent,
                fake_ref,
                gen_cnt: data.curr.gen_cnt,
            };
        }

        #[cold]
        fn load_slow<'a, T, D: DataPtrConvert<T>, const META_DATA_BITS: u32>(this: &'a SwapArcIntermediateTLS<T, D, { META_DATA_BITS }>, parent: &'a LocalData<T, D, META_DATA_BITS>, data: &mut LocalDataInner<T, D>) -> SwapArcIntermediateGuard<'a, T, D, META_DATA_BITS> {
            fn load_new_slow<'a, T, D: DataPtrConvert<T>, const META_DATA_BITS: u32>(this: &'a SwapArcIntermediateTLS<T, D, { META_DATA_BITS }>, data: &'a mut LocalDataInner<T, D>) -> (*const T, usize) {
                let curr = this.load_internal();
                // increase the strong reference count
                mem::forget(curr.fake_ref.clone());
                let new_ptr = curr.fake_ref.as_ptr();

                let gen_cnt = if data.curr.ref_cnt == 0 {
                    // don't modify the gen_cnt because we know that there are no references
                    // left to this (thread-)local instance
                    data.curr.refill_unchecked(new_ptr);
                    data.curr.gen_cnt
                } else {
                    if data.new.gen_cnt != 0 {
                        data.new.refill_unchecked(new_ptr);
                    } else {
                        let new = unsafe { LocalCounted::new(data, new_ptr) };
                        data.new = new;
                    }
                    data.new.gen_cnt
                };
                (new_ptr, gen_cnt)
            }

            // the following conditional relies on preconditions provided by the caller (`load`)
            // to be precise we rely on `parent.ptr` to be different from `curr.ptr`
            if data.new.ref_cnt == 0 {
                let (ptr, gen_cnt) = load_new_slow(this, data);
                // SAFETY: this is safe because we know that `load_new_slow` returns a pointer that
                // was acquired through `D::as_ptr` and points to a valid instance of `D`.
                let fake_ref = ManuallyDrop::new(unsafe { D::from(ptr) });
                return SwapArcIntermediateGuard {
                    parent,
                    fake_ref,
                    gen_cnt,
                };
            } else if unlikely(data.new.ref_cnt == usize::MAX) {
                // we now know that we are the first load to occur on the current thread
                data.new.ref_cnt = 0;
                // SAFETY: this is safe because we know that the pointer inside `data.curr.ptr`
                // was acquired through `D::as_ptr` and points to a valid instance of `D`
                // this is because the instance of `D` this pointer points to has to
                // have a `virtual reference` that points to it "stored" inside
                // `data.curr` i.e it has a reference to it leaked on `data.curr`'s
                // creation.
                let fake_ref = ManuallyDrop::new(unsafe { D::from(data.curr.ptr) });
                return SwapArcIntermediateGuard {
                    parent,
                    fake_ref,
                    gen_cnt: data.curr.gen_cnt,
                };
            }

            if data.curr.ref_cnt == 0 {
                // we can do the `curr` update on load because we know that we have a strong reference
                // to the value stored inside `ptr` anyways, so it doesn't really matter when exactly we update `curr`
                // and this allows us to save many branches on drop
                data.curr = mem::take(&mut data.new);
                if data.intermediate.ref_cnt != 0 {
                    // increase the ref count of the new value
                    mem::forget(data.intermediate.val().clone());
                    // SAFETY: we know that the reference count for the stored `D` got
                    // increased by us and thus we can decrease it again when
                    // it isn't needed anymore.
                    data.new = unsafe { mem::take(&mut data.intermediate).make_drop() };
                    parent.parent().intermediate_ref_cnt.fetch_sub(1, Ordering::AcqRel);
                    // SAFETY: this is safe because `data.new` was just updated by us and thus we know that it
                    // contains a valid ptr inside its `ptr` field.
                    let fake_ref = ManuallyDrop::new(unsafe { D::from(data.new.ptr) });
                    return SwapArcIntermediateGuard {
                        parent,
                        fake_ref,
                        gen_cnt: data.new.gen_cnt,
                    };
                }
                // FIXME: what do we do here? `curr` was replaced by `new` and `new` is empty now, do we return now, or do we allow a potential update of `intermediate`?
            }
            if data.intermediate.ref_cnt == 0 {
                // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                // as it, itself is protected by other atomics, so we can use `Relaxed`
                let intermediate = SwapArcIntermediateTLS::<T, D, { META_DATA_BITS }>::strip_metadata(parent.parent().intermediate_ptr.load(Ordering::Relaxed));
                // check if there is a new intermediate value and that the intermediate value has been verified to be usable
                let (ptr, gen_cnt) = if intermediate != data.new.ptr {
                    // there's a new value so we have to update our (thread-)local representation

                    // we have to make sure we observe the update of `intermediate_ref_cnt` correctly
                    fence(Ordering::Acquire);
                    // increment the reference count in order to avoid race conditions and act as a guard for our load
                    let loaded = parent.parent().intermediate_ref_cnt.fetch_add(1, Ordering::AcqRel);
                    // check if the update is still on-going or if the update is complete and we can thus use the
                    // new value
                    if loaded & SwapArcIntermediateTLS::<T, D, { META_DATA_BITS }>::UPDATE == 0 {
                        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                        // as it, itself is protected by other atomics, but  we need to make sure we never
                        // observe its guards' updates after its update, so we have to use `Acquire`
                        let loaded = SwapArcIntermediateTLS::<T, D, { META_DATA_BITS }>::strip_metadata(parent.parent().intermediate_ptr.load(Ordering::Relaxed));
                        if data.intermediate.gen_cnt != 0 {
                            data.intermediate.refill_unchecked(loaded);
                        } else {
                            let new = unsafe { LocalCounted::new(data, loaded) };
                            data.intermediate = new;
                        }
                        (loaded, data.intermediate.gen_cnt)
                    } else {
                        // the update is on-going, so we have to fallback to our locally most recent value
                        // and signal that the update finished.
                        parent.parent().intermediate_ref_cnt.fetch_sub(1, Ordering::Relaxed);
                        data.new.ref_cnt += 1;
                        (data.new.ptr, data.new.gen_cnt)
                    }
                } else {
                    // there's no new value so we can just return the newest one we have
                    data.new.ref_cnt += 1;
                    (data.new.ptr, data.new.gen_cnt)
                };
                // SAFETY: we loaded the `ptr` right before this and used local and global guards in order
                // to ensure that it's safe to dereference
                let fake_ref = ManuallyDrop::new(unsafe { D::from(ptr) });
                return SwapArcIntermediateGuard {
                    parent,
                    fake_ref,
                    gen_cnt,
                };
            } else {
                data.intermediate.ref_cnt += 1;
                // SAFETY: we just checked that the ref count of `intermediate` is greater than 0
                // and thus we know that `intermediate.ptr` has to be a valid value.
                let fake_ref = ManuallyDrop::new(unsafe { D::from(data.intermediate.ptr) });
                return SwapArcIntermediateGuard {
                    parent: &parent,
                    fake_ref,
                    gen_cnt: data.intermediate.gen_cnt,
                };
            }
        }

        load_slow(self, parent, data)
    }

    /// Loads the reference counted value that's currently stored
    /// inside the SwapArc fully i.e it increases the reference
    /// count directly.
    pub fn load_full(&self) -> D {
        self.load().as_ref().clone()
    }

    /// Loads the pointer with metadata into a guard which protects it partially.
    #[cfg(feature = "ptr-ops")]
    pub fn load_raw<'a>(&'a self) -> SwapArcIntermediatePtrGuard<'a, T, D, METADATA_BITS> {
        let guard = ManuallyDrop::new(self.load());
        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        let curr_meta = Self::get_metadata(self.intermediate_ptr.load(Ordering::Relaxed));
        SwapArcIntermediatePtrGuard {
            parent: guard.parent,
            // TODO: use this once strict provenance got stabilized!
            // ptr: guard.fake_ref.as_ptr().map_addr(|x| x | curr_meta),
            ptr: (guard.fake_ref.as_ptr() as usize | curr_meta) as *const T,
            gen_cnt: guard.gen_cnt,
        }
    }

    fn load_internal<'a>(&'a self) -> SwapArcIntermediateInternalGuard<'a, T, D, METADATA_BITS> {
        let ref_cnt = self.curr_ref_cnt.fetch_add(1, Ordering::AcqRel);
        let (ptr, src) = if ref_cnt & Self::UPDATE != 0 {
            let intermediate_ref_cnt = self.intermediate_ref_cnt.fetch_add(1, Ordering::AcqRel);
            if intermediate_ref_cnt & Self::UPDATE != 0 {
                // ORDERING: `ptr` doesn't have to care about anything other than itself
                // as it, itself is protected by other atomics, so we can use `Relaxed`
                let ret = self.ptr.load(Ordering::Relaxed);
                // release the redundant reference
                self.intermediate_ref_cnt.fetch_sub(1, Ordering::AcqRel);
                (ret, RefSource::Curr)
            } else {
                // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                // as it, itself is protected by other atomics, so we can use `Relaxed`
                let ret = self.intermediate_ptr.load(Ordering::Relaxed);
                // release the redundant reference
                self.curr_ref_cnt.fetch_sub(1, Ordering::AcqRel);
                (ret, RefSource::Intermediate)
            }
        } else {
            (self.ptr.load(Ordering::Relaxed), RefSource::Curr)
        };
        // create a fake reference to the Arc to ensure so that the borrow checker understands
        // that the reference returned from the guard will point to valid memory

        // SAFETY: this is safe because the pointers are protected by their reference counts
        // and the flags contained inside them.
        let fake_ref = ManuallyDrop::new(unsafe { D::from(Self::strip_metadata(ptr)) });
        SwapArcIntermediateInternalGuard {
            parent: self,
            fake_ref,
            ref_src: src,
            _no_send_guard: Default::default(),
        }
    }

    fn try_update_curr(&self) -> bool {
        let ret = match self.curr_ref_cnt.compare_exchange(Self::OTHER_UPDATE, Self::UPDATE, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => {
                // TODO: can we somehow bypass intermediate if we have a new update upcoming - we probably can't because this would probably cause memory leaks and other funny things that we don't like
                // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                // as it, itself is protected by other atomics, so we can use `Relaxed`
                let intermediate = self.intermediate_ptr.load(Ordering::Relaxed);
                // update the pointer
                // ORDERING: `ptr` doesn't have to care about anything other than itself
                // as it, itself is protected by other atomics, so we can use `Relaxed`
                let prev = self.ptr.load(Ordering::Relaxed);
                if Self::strip_metadata(prev) != Self::strip_metadata(intermediate) {
                    // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                    // as it, itself is protected by other atomics, so we can use `Relaxed`
                    self.ptr.store(intermediate, Ordering::Relaxed);
                    // unset the update flag
                    self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                    // drop the `virtual reference` we hold to the `D`

                    // SAFETY: we know that we hold a `virtual reference` to the `D`
                    // which `prev` points to and thus we are allowed to drop
                    // this `virtual reference`, once we replace `self.ptr` (or in other
                    // words `prev`)
                    unsafe { D::from(Self::strip_metadata(prev)); }
                } else {
                    // unset the update flag
                    self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                }
                // unset the `weak` update flag from the intermediate ref cnt
                self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::AcqRel);
                true
            }
            _ => false,
        };
        ret
    }

    fn try_update_intermediate(&self) {
        match self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => {
                // take the update
                let update = self.updated.swap(null_mut(), Ordering::AcqRel);
                // check if we even have an update
                if !update.is_null() {
                    // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                    // as it, itself is protected by other atomics, so we can use `Relaxed`
                    let metadata = Self::get_metadata(self.intermediate_ptr.load(Ordering::Relaxed));
                    let update = Self::merge_ptr_and_metadata(update, metadata).cast_mut();
                    // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                    // as it, itself is protected by other atomics, but when performing a load we have be
                    // able to ensure that we never observe the update of `intermediate_ptr`
                    // before the update of `intermediate_ref_cnt` in order to make the protection
                    // of it by the other atomics effective, so we have to use `Release`
                    self.intermediate_ptr.store(update, Ordering::Release);
                    // unset the update flag
                    self.intermediate_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                    // try finishing the update up!
                    match self.curr_ref_cnt.compare_exchange(0, Self::UPDATE, Ordering::AcqRel, Ordering::Relaxed) {
                        Ok(_) => {
                            // `ptr` doesn't have to care about anything other than itself
                            // as it, itself is protected by other atomics, so we can use `Relaxed`
                            let prev = self.ptr.load(Ordering::Relaxed);
                            self.ptr.store(update, Ordering::Relaxed);
                            // unset the update flag
                            self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                            // unset the `weak` update flag from the intermediate ref cnt
                            self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::AcqRel);
                            // drop the `virtual reference` we hold to the `D`

                            // SAFETY: we know that we hold a `virtual reference` to the `D`
                            // which `prev` points to and thus we are allowed to drop
                            // this `virtual reference`, once we replace `self.ptr` (or in other
                            // word `prev`)
                            unsafe { D::from(Self::strip_metadata(prev)); }
                        }
                        Err(_) => {}
                    }
                } else {
                    // unset the update flags
                    self.intermediate_ref_cnt.fetch_and(!(Self::UPDATE | Self::OTHER_UPDATE), Ordering::AcqRel);
                }
            }
            Err(_) => {}
        }
    }

    /// Update the value inside the SwapArc to value passed in `updated`.
    pub fn update(&self, updated: D) {
        // SAFETY: we know that `updated` is an instance of `D`, so we can generate a valid ptr to its content.
        unsafe { self._update_raw(updated.as_ptr()); }
    }

    /// SAFETY: `updated` has to be a pointer that points to a valid instance
    /// of `T` and has to be acquired by calling `D::as_ptr` or via similar means.
    #[cfg(feature = "ptr-ops")]
    pub unsafe fn update_raw(&self, updated: *const T) {
        self._update_raw(updated);
    }

    /// SAFETY: `updated` has to be a pointer that points to a valid instance
    /// of `T` and has to be acquired by calling `D::as_ptr` or via similar means.
    unsafe fn _update_raw(&self, updated: *const T) {
        // FIXME: add safety comments to this function!
        let updated = Self::strip_metadata(updated);
        let new = updated.cast_mut();
        // SAFETY: this is safe because the caller promises that `updated` is valid.
        let tmp = ManuallyDrop::new(D::from(new));
        // increase the ref count
        mem::forget(tmp.clone());
        let backoff = Backoff::new();
        loop {
            match self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    // clear out old updates to make sure our update won't be overwritten by them in the future
                    let old = self.updated.swap(null_mut(), Ordering::AcqRel);
                    // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                    // as it, itself is protected by other atomics, so we can use `Relaxed`
                    let metadata = Self::get_metadata(self.intermediate_ptr.load(Ordering::Relaxed));
                    let new = Self::merge_ptr_and_metadata(new, metadata).cast_mut();
                    self.intermediate_ptr.store(new, Ordering::Relaxed);
                    // unset the update flag
                    self.intermediate_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                    if !old.is_null() {
                        // drop the `virtual reference` we hold to the `D`
                        D::from(old);
                    }
                    // try finishing the update up!
                    loop {
                        let mut curr = 0;
                        match self.curr_ref_cnt.compare_exchange(curr, Self::UPDATE, Ordering::AcqRel, Ordering::Relaxed) {
                            Ok(_) => {
                                // `ptr` doesn't have to care about anything other than itself
                                // as it, itself is protected by other atomics, so we can use `Relaxed`
                                let prev = self.ptr.load(Ordering::Relaxed);
                                self.ptr.store(new, Ordering::Relaxed);
                                // unset the update flag
                                self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                                // unset the `weak` update flag from the intermediate ref cnt
                                self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::AcqRel);
                                // drop the `virtual reference` we hold to the `D`
                                D::from(Self::strip_metadata(prev));
                                break;
                            }
                            Err(_) => {
                                // signal that it's allowed to implicitly update this now

                                // ORDERING: the failure ordering of `Relaxed` can't race because we are performing an `Acquire`
                                // ordered atomic operation right here.
                                if self.curr_ref_cnt.fetch_or(Self::OTHER_UPDATE, Ordering::AcqRel) == 0 {
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
                    // ORDERING: this links with the cmp_exchg above as we only need syncing if we are actually doing something
                    // other than spinning
                    fence(Ordering::Acquire);
                    // push our update up, so it will be applied in the future
                    let old = self.updated.swap(new, Ordering::AcqRel); // FIXME: should we add some sort of update counter
                                                                                         // FIXME: to determine which update is the most recent?
                    if !old.is_null() {
                        // drop the `virtual reference` we hold to the `D`
                        D::from(old);
                    }
                    break;
                }
            }
        }
    }

    // TODO: maybe add some method along the lines of:
    // unsafe fn try_compare_exchange<const IGNORE_META: bool>(&self, old: *const T, new: D) -> Result<bool, D>

    // FIXME: this causes "deadlocks" if there are any other references alive
    pub unsafe fn try_compare_exchange_with_meta(&self, old: *const T, new: *const T) -> bool {
        // FIXME: add safety comments!
        let backoff = Backoff::new();
        while !self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            // back-off
            backoff.snooze();
        }
        // This load may be `Relaxed` because it is guarded by the `intermediate_ref_cnt`.
        let intermediate = self.intermediate_ptr.load(Ordering::Relaxed);
        if intermediate.cast_const() != old {
            // ORDERING: We use Acquire here because we have a Release above with which we can establish
            // a happens-before relationship with the RMW (compare_exchange) above
            self.intermediate_ref_cnt.fetch_and(!(Self::UPDATE | Self::OTHER_UPDATE), Ordering::AcqRel/*Ordering::Acquire*/);
            return false;
        }
        // clear out old updates to make sure our update won't be overwritten by them in the future
        let old_update = self.updated.swap(null_mut(), Ordering::AcqRel);
        // increase the ref count
        let tmp = ManuallyDrop::new(D::from(Self::strip_metadata(new)));
        mem::forget(tmp.clone());
        self.intermediate_ptr.store(new.cast_mut(), Ordering::Relaxed);
        // unset the update flag
        self.intermediate_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
        if !old_update.is_null() {
            // drop the `virtual reference` we hold to the Arc
            D::from(old_update);
        }
        let mut curr = 0;
        loop {
            match self.curr_ref_cnt.compare_exchange(curr, Self::UPDATE, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    // ORDERING: Relaxed should be okay because curr_ref_cnt guards `ptr`
                    // and ensures that other threads see the update because
                    // of the happens-before-relationship between
                    let prev = self.ptr.load(Ordering::Relaxed);
                    self.ptr.store(new.cast_mut(), Ordering::Relaxed);
                    // unset the update flag
                    self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                    // unset the `weak` update flag from the intermediate ref cnt
                    self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::AcqRel);
                    // drop the `virtual reference` we hold to the Arc
                    D::from(Self::strip_metadata(prev));
                    break;
                }
                Err(_) => {
                    // signal that it's allowed to implicitly update this now
                    if self.curr_ref_cnt.fetch_or(Self::OTHER_UPDATE, Ordering::AcqRel) == 0 {
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

    /// Updates the currently stored metadata with the new
    /// metadata passed in the `metadata` parameter.
    pub fn update_metadata(&self, metadata: usize) {
        let backoff = Backoff::new();
        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        let mut curr = self.intermediate_ptr.load(Ordering::Relaxed);
        loop {
            let prefix = metadata & Self::META_MASK;

            // TODO: use this once strict provenance got stabilized
            // match self.intermediate_ptr.compare_exchange_weak(curr, curr.map_addr(|x| (x & !Self::META_MASK) | prefix), Ordering::Relaxed, Ordering::Relaxed) {

            // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
            // as it, itself is protected by other atomics, so we can use `Relaxed`
            match self.intermediate_ptr.compare_exchange_weak(curr, ((curr as usize & !Self::META_MASK) | prefix) as *mut T, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(err) => {
                    if is_cmp_exchg_really_weak() && curr == err {
                        // the fail was spurious
                        continue;
                    }
                    curr = err;
                }
            }

            backoff.spin(); // FIXME: should we really backoff here? the other thread will make progress anyways and we will only have to spin once more if it makes progress again
        }
    }

    /// Tries to replace the current metadata with the new one if there was no update in between.
    /// `old` is the previous pointer and should contain the previous metadata.
    /// `metadata` is the new metadata, the old one will be replaced with.
    #[cfg(feature = "ptr-ops")]
    pub fn try_update_meta(&self, old: *const T, metadata: usize) -> bool {
        let prefix = metadata & Self::META_MASK;
        // TODO: use this once strict provenance got stabilized
        // self.intermediate_ptr.compare_exchange(old.cast_mut(), old.map_addr(|x| (x & !Self::META_MASK) | prefix).cast_mut(), Ordering::Relaxed, Ordering::Relaxed).is_ok()

        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        self.intermediate_ptr.compare_exchange(old.cast_mut(), ((old as usize & !Self::META_MASK) | prefix) as *mut T, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    }

    /// Sets all bits of the internal pointer which are set in the `inactive_bits` parameter
    pub fn set_in_metadata(&self, active_bits: usize) {
        let backoff = Backoff::new();
        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        let mut curr = self.intermediate_ptr.load(Ordering::Relaxed);
        loop {
            let prefix = active_bits & Self::META_MASK;
            // TODO: use this once strict provenance got stabilized
            // match self.intermediate_ptr.compare_exchange_weak(curr, curr.map_addr(|x| x | prefix), Ordering::Relaxed, Ordering::Relaxed) {

            // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
            // as it, itself is protected by other atomics, so we can use `Relaxed`
            match self.intermediate_ptr.compare_exchange_weak(curr, (curr as usize | prefix) as *mut T, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(err) => {
                    if is_cmp_exchg_really_weak() && curr == err {
                        // the fail was spurious
                        continue;
                    }
                    curr = err;
                }
            }
            backoff.spin(); // FIXME: should we really backoff here? the other thread will make progress anyways and we will only have to spin once more if it makes progress again
        }
    }

    /// Unsets all bits of the internal pointer which are set in the `inactive_bits` parameter
    pub fn unset_in_metadata(&self, inactive_bits: usize) {
        let backoff = Backoff::new();
        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        let mut curr = self.intermediate_ptr.load(Ordering::Relaxed);
        loop {
            let prefix = inactive_bits & Self::META_MASK;
            // TODO: use this once strict provenance got stabilized!
            // match self.intermediate_ptr.compare_exchange_weak(curr, curr.map_addr(|x| x & !prefix), Ordering::Relaxed, Ordering::Relaxed) {

            // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
            // as it, itself is protected by other atomics, so we can use `Relaxed`
            match self.intermediate_ptr.compare_exchange_weak(curr, (curr as usize & !prefix) as *mut T, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(err) => {
                    if is_cmp_exchg_really_weak() && err == curr {
                        // the fail was spurious
                        continue;
                    }
                    curr = err;
                }
            }
            backoff.spin(); // FIXME: should we really backoff here? the other thread will make progress anyways and we will only have to spin once more if it makes progress again
        }
    }

    /// Returns the metadata stored inside the internal pointer
    pub fn load_metadata(&self) -> usize {
        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        self.intermediate_ptr.load(Ordering::Relaxed) as usize & Self::META_MASK
    }

    #[inline(always)]
    fn get_metadata(ptr: *const T) -> usize {
        ptr as usize & Self::META_MASK
    }

    #[inline(always)]
    fn strip_metadata(ptr: *const T) -> *const T {
        // TODO: use this once strict_provenance has been stabilized
        // ptr.map_addr(|x| x & !Self::META_MASK)
        (ptr as usize & !Self::META_MASK) as *const T
    }

    #[inline(always)]
    fn merge_ptr_and_metadata(ptr: *const T, metadata: usize) -> *const T {
        // TODO: use this once strict_provenance has been stabilized
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

    // TODO: MAYBE: introduce FORCE_UPDATEing into the data structure in order for the user to be able
    // to ensure that there's NO update pending

}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop for SwapArcIntermediateTLS<T, D, METADATA_BITS> {
    fn drop(&mut self) {
        // FIXME: how should we handle intermediate inside drop?
        let updated = *self.updated.get_mut();
        if !updated.is_null() {
            // SAFETY: we know that we hold a `virtual reference` to the `D`
            // which `updated` points to and thus we are allowed to drop
            // this `virtual reference`, once we drop the `SwapArc`.
            unsafe { D::from(updated); }
        }
        let curr = Self::strip_metadata(*self.ptr.get_mut());
        let intermediate = Self::strip_metadata(*self.intermediate_ptr.get_mut());
        if intermediate != curr {
            // FIXME: the reason why we have to do this currently is because the update function doesn't work properly, fix the root cause!
            unsafe { D::from(intermediate); }
        }
        // drop the current `D`

        // SAFETY: we know that we hold a `virtual reference` to the `D`
        // which `curr` points to and thus we are allowed to drop
        // this `virtual reference`, once we drop the `SwapArc`.
        unsafe { D::from(curr); }
    }
}

cfg_if! {
    if #[cfg(feature = "ptr-ops")] {
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
                self.parent.parent().load_raw()
            }
        }

        impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop for SwapArcIntermediatePtrGuard<'_, T, D, METADATA_BITS> {
            fn drop(&mut self) {
                // SAFETY: This is safe because we know that we are the only thread that
                // is able to access the thread local data at this time and said data has to be initialized
                // and we also know, that the pointer has to be non-null
                let data = unsafe { self.parent.inner.get().as_mut().unwrap_unchecked() };
                // release the reference we hold
                if likely(self.gen_cnt == data.curr.gen_cnt) {
                    data.curr.ref_cnt -= 1;
                } else {
                    slow_drop(self.parent, data, self.gen_cnt);
                }
                #[cold]
                fn slow_drop<T, D: DataPtrConvert<T>, const METADATA_BITS: u32>(parent: &LocalData<T, D, METADATA_BITS>, data: &mut LocalDataInner<T, D>, gen_cnt: usize) {
                    if gen_cnt == data.new.gen_cnt {
                        data.new.ref_cnt -= 1;
                        if data.new.ref_cnt == 0 {
                            if data.intermediate.ref_cnt != 0 {
                                // increase the ref count of the new value
                                mem::forget(data.intermediate.val().clone());
                                // SAFETY: we know that the reference count for the stored `D` got
                                // increased by us and thus we can decrease it again when
                                // it isn't needed anymore.
                                data.new = unsafe { mem::take(&mut data.intermediate).make_drop() };
                                parent.parent().intermediate_ref_cnt.fetch_sub(1, Ordering::AcqRel);
                            }
                        }
                    } else {
                        data.intermediate.ref_cnt -= 1;
                        if data.intermediate.ref_cnt == 0 {
                            parent.parent().intermediate_ref_cnt.fetch_sub(1, Ordering::AcqRel);
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
        let data = unsafe { self.parent.inner.get().as_mut().unwrap_unchecked() };
        // release the reference we hold
        if likely(self.gen_cnt == data.curr.gen_cnt) {
            data.curr.ref_cnt -= 1;
        } else {
            slow_drop(self.parent, data, self.gen_cnt);
        }
        #[cold]
        fn slow_drop<T, D: DataPtrConvert<T>, const METADATA_BITS: u32>(parent: &LocalData<T, D, METADATA_BITS>, data: &mut LocalDataInner<T, D>, gen_cnt: usize) {
            if gen_cnt == data.new.gen_cnt {
                data.new.ref_cnt -= 1;
                if data.new.ref_cnt == 0 {
                    if data.intermediate.ref_cnt != 0 {
                        // increase the ref count of the new value
                        mem::forget(data.intermediate.val().clone());
                        // SAFETY: we know that the reference count for the stored `D` got
                        // increased by us and thus we can decrease it again when
                        // it isn't needed anymore.
                        data.new = unsafe { mem::take(&mut data.intermediate).make_drop() };
                        parent.parent().intermediate_ref_cnt.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            } else {
                data.intermediate.ref_cnt -= 1;
                if data.intermediate.ref_cnt == 0 {
                    parent.parent().intermediate_ref_cnt.fetch_sub(1, Ordering::AcqRel);
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
    parent: &'a SwapArcIntermediateTLS<T, D, METADATA_BITS>,
    fake_ref: ManuallyDrop<D>,
    ref_src: RefSource,
    _no_send_guard: PhantomData<RefCell<()>>, // this ensures that this struct is not sent over to other threads
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop for SwapArcIntermediateInternalGuard<'_, T, D, METADATA_BITS> {
    fn drop(&mut self) {
        // release the reference we hold
        match self.ref_src {
            RefSource::Curr => {
                let ref_cnt = self.parent.curr_ref_cnt.fetch_sub(1, Ordering::AcqRel);
                if ref_cnt == SwapArcIntermediateTLS::<T, D, METADATA_BITS>::OTHER_UPDATE + 1/*1*/ { // FIXME: is it okay to do nothing on 1?
                    self.parent.try_update_curr();
                }
            }
            RefSource::Intermediate => {
                let ref_cnt = self.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::AcqRel);
                // fast-rejection path to ensure we are only trying to update if it's worth it
                if ref_cnt == 1 && !self.parent.updated.load(Ordering::Relaxed).is_null() {
                    self.parent.try_update_intermediate();
                }
            }
        }
    }
}

enum RefSource {
    Curr,
    Intermediate,
}

struct LocalData<T, D: DataPtrConvert<T> = Arc<T>, const METADATA_BITS: u32 = 0> {
    parent: *const SwapArcIntermediateTLS<T, D, METADATA_BITS>, // this acts as a reference with hidden lifetime that only we know is safe because
                                                                // `parent` won't be used in the drop impl and `LocalData` can only be accessed
                                                                // through `parent`
    inner: UnsafeCell<LocalDataInner<T, D>>,
}

impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> LocalData<T, D, METADATA_BITS> {

    #[inline]
    fn parent(&self) -> &SwapArcIntermediateTLS<T, D, METADATA_BITS> {
        // SAFETY: we know that `parent` has to be alive at this point
        // because otherwise `LocalData` wouldn't be alive.
        unsafe { self.parent.as_ref().unwrap_unchecked() }
    }

}

// SAFETY: this is safe because `parent` has to be alive when `LocalData` gets used
// as it is only reachable through a valid reference to `SwapArc`
unsafe impl<T, D: DataPtrConvert<T>, const METADATA_BITS: u32> Send for LocalData<T, D, METADATA_BITS> {}

struct LocalDataInner<T, D: DataPtrConvert<T> = Arc<T>> {
    next_gen_cnt: usize,
    intermediate: LocalCounted<T, D>,
    new: LocalCounted<T, D, true>,
    curr: LocalCounted<T, D, true>,
}

struct LocalCounted<T, D: DataPtrConvert<T> = Arc<T>, const DROP: bool = false> {
    gen_cnt: usize, // TODO: would it help somehow (the performance) if we were to make `gen_cnt` an `u8`?
    ptr: *const T,
    ref_cnt: usize,
    _phantom_data: PhantomData<D>,
}

impl<T, D: DataPtrConvert<T>, const DROP: bool> LocalCounted<T, D, DROP> {

    /// SAFETY: The caller has to ensure that the safety invariants relied upon
    /// in the `val`, `refill_unchecked` and `drop` methods are valid.
    unsafe fn new(parent: &mut LocalDataInner<T, D>, ptr: *const T) -> Self {
        let gen_cnt = parent.next_gen_cnt;
        let res = parent.next_gen_cnt.overflowing_add(1);
        // if an overflow occurs, we add an additional 1 to the result in order to never
        // reach 0
        parent.next_gen_cnt = res.0 + res.1 as usize;
        Self {
            gen_cnt,
            ptr,
            ref_cnt: 1,
            _phantom_data: Default::default(),
        }
    }

    #[inline]
    fn val(&self) -> ManuallyDrop<D> {
        // SAFETY: this is safe because we know that the pointer
        // we hold has to be valid as long as our `gen_cnt` isn't `0`.
        ManuallyDrop::new(unsafe { D::from(self.ptr) })
    }

    fn refill_unchecked(&mut self, ptr: *const T) {
        if DROP {
            if !self.ptr.is_null() {
                // SAFETY: the person defining this struct has to make sure that
                // choosing `DROP` is correct.
                unsafe { D::from(self.ptr); }
            }
        }
        self.ptr = ptr;
        self.ref_cnt = 1;
    }

}

impl<T, D: DataPtrConvert<T>> LocalCounted<T, D, false> {

    /// SAFETY: callers have to make sure that the value behind the `ptr`
    /// contained inside this struct may be dropped as soon as this struct
    /// gets dropped.
    #[inline]
    unsafe fn make_drop(self) -> LocalCounted<T, D, true> {
        LocalCounted {
            gen_cnt: self.gen_cnt,
            ptr: self.ptr,
            ref_cnt: self.ref_cnt,
            _phantom_data: Default::default(),
        }
    }

}

impl<T, D: DataPtrConvert<T>, const DROP: bool> Default for LocalCounted<T, D, DROP> {
    #[inline]
    fn default() -> Self {
        Self {
            gen_cnt: 0,
            ptr: null(),
            ref_cnt: 0,
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
            if !self.ptr.is_null() {
                // SAFETY: the person defining this struct has to make sure that
                // choosing `DROP` is correct.
                unsafe { D::from(self.ptr); }
            }
        }
    }
}

/// SAFETY: Types implementing this trait are expected to perform
/// reference counting through cloning/dropping internally.
/// To be precise, the `RefCnt` is expected to increment
/// the reference count on `clone` calls, and decrement
/// it on `drop`.
pub unsafe trait RefCnt: /*Send + Sync + */Clone {} // FIXME: is not having a Send + Sync bound correct here?

pub trait DataPtrConvert<T>: RefCnt + Sized {

    /// This function may not alter the reference count of the
    /// reference counted "object".
    unsafe fn from(ptr: *const T) -> Self;

    /// This function should decrement the reference count of the
    /// reference counted "object" indirectly, by automatically
    /// decrementing it on drop inside the "object"'s drop
    /// implementation.
    #[inline]
    fn into(self) -> *const T {
        self.as_ptr()
    }

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

    fn as_ptr(&self) -> *const T {
        match self {
            None => null(),
            Some(val) => Arc::as_ptr(val),
        }
    }
}

/// This function indicates whether the current target does really
/// have a weak compare exchange method.
/// NOTE: It **Must not** be relied upon for correctness.
#[inline(always)]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
const fn is_cmp_exchg_really_weak() -> bool {
    false
}

/// This function indicates whether the current target does really
/// have a weak compare exchange method.
/// NOTE: It **Must not** be relied upon for correctness.
#[inline(always)]
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
const fn is_cmp_exchg_really_weak() -> bool {
    true
}
