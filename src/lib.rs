use cfg_if::cfg_if;
use crossbeam_utils::{Backoff, CachePadded};
use likely_stable::{likely, unlikely};
use std::borrow::Borrow;
use std::cell::{RefCell, UnsafeCell};
use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;
use std::mem;
use std::mem::{align_of, ManuallyDrop};
use std::ops::Deref;
use std::ptr::{null, null_mut};
use std::sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;
use thread_local::ThreadLocal;

pub type SwapArc<T> = SwapArcAnyMeta<T, Arc<T>, 0>;
pub type SwapArcOption<T> = SwapArcAnyMeta<T, Option<Arc<T>>, 0>;
pub type SwapArcOptionMeta<T, const METADATA_BITS: u32> =
    SwapArcAnyMeta<T, Option<Arc<T>>, METADATA_BITS>;
pub type SwapArcAny<T, D> = SwapArcAnyMeta<T, D, 0>;

// TODO: we can probably combine the counters and ptrs in the main struct into an [(AtomicUsize, AtomicPtr); 2] and have
// TODO: a third number that is either 0 or 1 and represents an index into the array, this could improve performance!

/// A `SwapArc` is a data structure that allows for an `Arc`
/// to be passed around and swapped out with other `Arc`s.
/// In order to achieve this, an internal reference count
/// scheme is used which allows for very quick, low overhead
/// reads in the common case (no update) and will still be
/// decently fast when an update is performed. When a new
/// `Arc` is to be stored in the `SwapArc`, it first tries to
/// immediately update the current pointer (the one all `strong readers` will see in the uncontended case)
/// (this is possible, if no other update is being performed and if there are no `strong readers` left)
/// if this fails, it will try to do the same thing with the `intermediate` pointer
/// and if even that fails, it will `push` the update so that it will
/// be performed by the last `intermediate` reader to finish reading.
/// A `strong read` consists of loading the current pointer and
/// performing a clone operation on the `Arc`, thus
/// `strong readers` are very short-lived and can't block
/// updates for very long.
/// A `strong read` in this case refers to the process of acquiring
/// the shared pointer which gets handled by the `SwapArc`
/// itself internally. Note that such a `strong read` isn't what would
/// typically happen when `load` gets called as such a call
/// usually (in the case of no update) will only result
/// in a `weak read` and thus only
/// in a single `Relaxed` atomic load and a couple of
/// non-atomic bookkeeping operations utilizing TLS
/// to cache previous reads.

/// Note: `SwapArc` has wait-free reads.
pub struct SwapArcAnyMeta<
    T: Send + Sync,
    D: DataPtrConvert<T> = Arc<T>,
    const METADATA_BITS: u32 = 0,
> {
    curr_ref_cnt: AtomicUsize, // the last bit is the `update` bit
    ptr: AtomicPtr<T>,
    intermediate_ref_cnt: AtomicUsize, // the last bit is the `update` bit
    intermediate_ptr: AtomicPtr<T>,
    updated: AtomicPtr<T>,
    thread_local: ThreadLocal<CachePadded<LocalData<T, D, METADATA_BITS>>>,
}

#[inline]
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

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32>
    SwapArcAnyMeta<T, D, METADATA_BITS>
{
    const UPDATE: usize = 1 << (usize::BITS - 1); // when this bit is set on a cnt, it means that it is currently being
                                                  // updated and its ptr may not be used while this bit is set.
    const OTHER_UPDATE: usize = 1 << (usize::BITS - 2); // when this bit is set on `intermediate` we know that its value hasn't propagated to `ptr` yet.
                                                        // and when it's set on `curr` we know that curr still has to be updated

    /// Creates a new `SwapArc` instance with `val` as the initial value.
    pub fn new(val: D) -> Self {
        // TODO: use `inline_const` once it gets stabilized in order to
        // TODO: make sure it gets optimized away.
        let free_bits = most_sig_set_bit(align_of::<T>()).unwrap_or(0);
        if free_bits < METADATA_BITS {
            let expected = 1 << METADATA_BITS;
            let found = 1 << free_bits;
            panic!(
                "The alignment of T is insufficient, expected `{}`, but found `{}`",
                expected, found
            );
        }
        let virtual_ref = val.into_ptr();
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
        // TODO: use `inline_const` once it gets stabilized in order to
        // TODO: make sure it gets optimized away.
        let free_bits = most_sig_set_bit(align_of::<T>()).unwrap_or(0);
        if free_bits < METADATA_BITS {
            let expected = 1 << METADATA_BITS;
            let found = 1 << free_bits;
            panic!(
                "The alignment of T is insufficient, expected `{}`, but found `{}`",
                expected, found
            );
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
    pub fn load(&self) -> SwapArcGuard<T, D, METADATA_BITS> {
        let parent = self.thread_local.get_or(|| {
            let curr = self.load_strongly();
            // increase the reference count
            let curr_ptr = ManuallyDrop::into_inner(curr.fake_ref.clone()).into_ptr();
            CachePadded::new(LocalData {
                // SAFETY: this is safe because we know that this will only ever be accessed
                // if there is a live reference to `self` present.
                parent: self as *const Self,
                inner: UnsafeCell::new(LocalDataInner {
                    next_gen_cnt: 2,
                    intermediate: LocalCounted::default(),
                    new: LocalCounted {
                        gen_cnt: 0,
                        ptr: null_mut(),
                        ref_cnt: usize::MAX,
                        _phantom_data: Default::default(),
                    },
                    curr: LocalCounted {
                        gen_cnt: 1,
                        ptr: curr_ptr.cast_mut(),
                        ref_cnt: 1,
                        _phantom_data: Default::default(),
                    },
                }),
            })
        });
        // SAFETY: This is safe because we know that we are the only thread that
        // is able to access the thread local data at this time and said data has to be initialized
        // and we also know, that the pointer has to be non-null
        let data = unsafe { parent.inner.get().as_mut().unwrap_unchecked() };

        // ORDERING: `ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        let no_update =
            Self::strip_metadata(parent.parent().ptr.load(Ordering::Relaxed)) == data.curr.ptr;
        if likely(no_update && data.new.ref_cnt == 0) {
            data.curr.ref_cnt += 1;
            // SAFETY: this is safe because the pointer contained inside `curr.ptr` is guaranteed to always be valid
            let fake_ref = ManuallyDrop::new(unsafe { D::from(data.curr.ptr) });
            return SwapArcGuard {
                parent,
                fake_ref,
                gen_cnt: data.curr.gen_cnt,
            };
        }

        #[cold]
        fn load_slow<'a, T: Send + Sync, D: DataPtrConvert<T>, const META_DATA_BITS: u32>(
            this: &'a SwapArcAnyMeta<T, D, { META_DATA_BITS }>,
            parent: &'a LocalData<T, D, META_DATA_BITS>,
            data: &mut LocalDataInner<T, D>,
        ) -> SwapArcGuard<'a, T, D, META_DATA_BITS> {
            fn load_updated_slow<
                'a,
                T: Send + Sync,
                D: DataPtrConvert<T>,
                const META_DATA_BITS: u32,
            >(
                this: &'a SwapArcAnyMeta<T, D, { META_DATA_BITS }>,
                data: &'a mut LocalDataInner<T, D>,
            ) -> (*const T, usize) {
                let curr = this.load_strongly();
                // increase the strong reference count
                let new_ptr = ManuallyDrop::into_inner(curr.fake_ref.clone()).into_ptr();

                // check if we can immediately update our `curr` value or if we have to
                // put the update into `new` until `curr` can be updated.
                let gen_cnt = if data.curr.ref_cnt == 0 {
                    // don't modify the gen_cnt because we know that there are no references
                    // left to this (thread-)local instance
                    data.curr.refill_unchecked(new_ptr);
                    data.curr.gen_cnt
                } else {
                    if data.new.gen_cnt != 0 {
                        // don't modify the gen_cnt because we know that there are no references
                        // left to this (thread-)local instance
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
            // to be precise we rely on `parent.ptr` to be different from `data.curr.ptr`
            if data.new.ref_cnt == 0 {
                let (ptr, gen_cnt) = load_updated_slow(this, data);
                // SAFETY: this is safe because we know that `load_new_slow` returns a pointer that
                // was acquired through `D::as_ptr` and points to a valid instance of `D`.
                let fake_ref = ManuallyDrop::new(unsafe { D::from(ptr) });
                return SwapArcGuard {
                    parent,
                    fake_ref,
                    gen_cnt,
                };
            }

            if unlikely(data.new.ref_cnt == usize::MAX) {
                // we now know that we are the first load to occur on the current thread
                data.new.ref_cnt = 0;
                // SAFETY: this is safe because we know that the pointer inside `data.curr.ptr`
                // was acquired through `D::as_ptr` and points to a valid instance of `D`
                // this is because the instance of `D` this pointer points to has to
                // have a `virtual reference` that points to it "stored" inside
                // `data.curr` i.e it has a reference to it leaked on `data.curr`'s
                // creation.
                let fake_ref = ManuallyDrop::new(unsafe { D::from(data.curr.ptr) });
                return SwapArcGuard {
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
                // if the `ref_cnt` of `intermediate` is not `0`, we know that its `ptr` is valid
                if data.intermediate.ref_cnt != 0 {
                    // increase the ref count of the new value
                    data.intermediate.val().increase_ref_cnt();
                    // SAFETY: we know that the reference count for the stored `D` got
                    // increased by us and thus we can decrease it again when
                    // it isn't needed anymore.
                    data.new = unsafe { mem::take(&mut data.intermediate).make_drop() };
                    parent
                        .parent()
                        .intermediate_ref_cnt
                        .fetch_sub(1, Ordering::AcqRel);
                    // SAFETY: this is safe because `data.new` was just updated by us and thus we know that it
                    // contains a valid ptr inside its `ptr` field.
                    let fake_ref = ManuallyDrop::new(unsafe { D::from(data.new.ptr) });
                    return SwapArcGuard {
                        parent,
                        fake_ref,
                        gen_cnt: data.new.gen_cnt,
                    };
                }

                // curr was replace by `new` and `new` is empty now, so we return to not allow
                // for `intermediate` to be updated because that would mean that we have an
                // unexpected state in which we have:
                // `curr`: value
                // `new` : empty
                // `intermediate`: value
                // to avoid this we try to update `new` or just return `curr` if we fail to do so

                // TODO: do we always have to assume that there is an update pending or can we check if there actually is?
                let curr = this.load_strongly();

                // SAFETY: we know that `curr.ptr` has to be valid because we just moved
                // `new` to `curr` with `new`'s `ref_cnt` being non-zero (which means that
                // `new.ptr` had to have been valid)
                let other = ManuallyDrop::new(unsafe { D::from(data.curr.ptr) });
                if core::ptr::eq(curr.fake_ref.deref() as *const D, other.deref() as *const D) {
                    // there is no new update, just stick with the version we have

                    data.curr.ref_cnt += 1;
                    // SAFETY: this is safe because the pointer contained inside `curr.ptr` is guaranteed to always be valid
                    let fake_ref = ManuallyDrop::new(unsafe { D::from(data.curr.ptr) });
                    return SwapArcGuard {
                        parent,
                        fake_ref,
                        gen_cnt: data.curr.gen_cnt,
                    };
                }
                // increase the strong reference count
                let new_ptr = ManuallyDrop::into_inner(curr.fake_ref.clone()).into_ptr();

                // don't modify the gen_cnt because we know that there are no references
                // left to this (thread-)local instance
                data.new.refill_unchecked(new_ptr);

                return SwapArcGuard {
                    parent,
                    // SAFETY: This is safe because we just created the instance `new_ptr` is pointing to
                    // and we didn't delete it. Thus `new_ptr` is a ptr to a valid instance of `D`.
                    fake_ref: ManuallyDrop::new(unsafe { D::from(new_ptr) }),
                    gen_cnt: data.new.gen_cnt,
                };
            }

            if data.intermediate.ref_cnt != 0 {
                data.intermediate.ref_cnt += 1;
                // SAFETY: we just checked that the ref count of `intermediate` is greater than 0
                // and thus we know that `intermediate.ptr` has to be a valid value.
                let fake_ref = ManuallyDrop::new(unsafe { D::from(data.intermediate.ptr) });
                return SwapArcGuard {
                    parent: &parent,
                    fake_ref,
                    gen_cnt: data.intermediate.gen_cnt,
                };
            }

            // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
            // as it, itself is protected by other atomics, so we can use `Relaxed`
            let intermediate = SwapArcAnyMeta::<T, D, { META_DATA_BITS }>::strip_metadata(
                parent.parent().intermediate_ptr.load(Ordering::Relaxed),
            );
            // check if there is a new intermediate value and that the intermediate value has been verified to be usable
            let (ptr, gen_cnt) = if intermediate != data.new.ptr {
                // there's a new value so we have to update our (thread-)local representation

                // we have to make sure we observe the update of `intermediate_ref_cnt` correctly
                fence(Ordering::Acquire);
                // increment the reference count in order to avoid race conditions and act as a guard for our load
                let loaded = parent
                    .parent()
                    .intermediate_ref_cnt
                    .fetch_add(1, Ordering::AcqRel);
                // check if the update is still on-going or if the update is complete and we can thus use the
                // new value
                if loaded & SwapArcAnyMeta::<T, D, { META_DATA_BITS }>::UPDATE == 0 {
                    // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                    // as it, itself is protected by other atomics, but  we need to make sure we never
                    // observe its guards' updates after its update, so we have to use `Acquire`
                    let loaded = SwapArcAnyMeta::<T, D, { META_DATA_BITS }>::strip_metadata(
                        parent.parent().intermediate_ptr.load(Ordering::Relaxed),
                    );
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
                    parent
                        .parent()
                        .intermediate_ref_cnt
                        .fetch_sub(1, Ordering::Relaxed);
                    data.new.ref_cnt += 1;
                    (data.new.ptr.cast_const(), data.new.gen_cnt)
                }
            } else {
                // there's no new value so we can just return the newest one we have
                data.new.ref_cnt += 1;
                (data.new.ptr.cast_const(), data.new.gen_cnt)
            };
            // SAFETY: we loaded the `ptr` right before this and used local and global guards in order
            // to ensure that it's safe to dereference
            let fake_ref = ManuallyDrop::new(unsafe { D::from(ptr) });
            return SwapArcGuard {
                parent,
                fake_ref,
                gen_cnt,
            };
        }

        load_slow(self, parent, data)
    }

    /// Loads the reference counted value that's currently stored
    /// inside the SwapArc strongly i.e it increases the reference
    /// count directly.
    /// This is slower than `load` but will allow for the result
    /// to be used even after the `SwapArc` was dropped.
    pub fn load_full(&self) -> D {
        self.load().as_ref().clone()
    }

    /// Loads the pointer with metadata into a guard which protects it weakly.
    /// The protection offered by this method is the same as the protection
    /// offered by `load`.
    /// Note: `weak` in this context doesn't mean that it's less safe to use
    /// this method compared to `load_full`, rather it means that it **should**
    /// be used only for short-lived usages - long-lived usages won't introduce
    /// **any** unsafety tho.
    #[cfg(feature = "ptr-ops")]
    pub fn load_raw(&self) -> SwapArcPtrGuard<T, D, METADATA_BITS> {
        let guard = ManuallyDrop::new(self.load());
        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        let curr_meta = Self::get_metadata(self.intermediate_ptr.load(Ordering::Relaxed));
        // SAFETY: This is safe as we know that the ptr is valid and we are *not* dropping the
        // inner value we read from it.
        let ptr =
            ManuallyDrop::into_inner(unsafe { (&guard.fake_ref as *const ManuallyDrop<D>).read() })
                .into_ptr();
        SwapArcPtrGuard {
            parent: guard.parent,
            ptr: ptr::map_addr(ptr, |x| x | curr_meta),
            gen_cnt: guard.gen_cnt,
        }
    }

    /// Loads the reference counted value that's currently stored
    /// inside the SwapArc strongly i.e it increases the reference
    /// count directly.
    /// This is slower than `load` but will allow for the result
    /// to be used even after the `SwapArc` was dropped.
    #[cfg(feature = "ptr-ops")]
    pub fn load_raw_full(&self) -> SwapArcFullPtrGuard<T, D, METADATA_BITS> {
        let (ptr, src) = self.load_internal();

        let val = ManuallyDrop::new(unsafe { D::from(ptr) });
        let val = ManuallyDrop::into_inner(val.clone());

        match src {
            RefSource::Curr => {
                self.curr_ref_cnt.fetch_sub(1, Ordering::Release);
            }
            RefSource::Intermediate => {
                self.intermediate_ref_cnt.fetch_sub(1, Ordering::Release);
            }
        }

        SwapArcFullPtrGuard {
            inner: val,
            ptr,
        }
    }

    /// This is what the docs are referring to as a `strong read` or
    /// `strong load`.
    fn load_strongly(&self) -> SwapArcStrongGuard<T, D, METADATA_BITS> {
        let (ptr, src) = self.load_internal();
        // create a fake reference to the `D` to ensure so that the borrow checker understands
        // that the reference returned from the guard will point to valid memory

        // SAFETY: this is safe because the pointers are protected by their reference counts
        // and the flags contained inside them.
        let fake_ref = ManuallyDrop::new(unsafe { D::from(Self::strip_metadata(ptr)) });
        SwapArcStrongGuard {
            parent: self,
            fake_ref,
            ref_src: src,
            _no_send_guard: Default::default(),
        }
    }

    fn load_internal(&self) -> (*mut T, RefSource) {
        let ref_cnt = self.curr_ref_cnt.fetch_add(1, Ordering::AcqRel);
        // Check if there is currently an update of `curr` on-going because if there is,
        // the ptr can be invalidated at any point in time. Additionally we have to check
        // if there is an implicit update allowed which also means that the ptr
        // can be invalidated at any point in time.
        if ref_cnt & (Self::UPDATE | Self::OTHER_UPDATE) != 0 {
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
        }
    }

    #[cold]
    fn try_update_curr(&self) -> bool {
        match self.curr_ref_cnt.compare_exchange(
            Self::OTHER_UPDATE,
            Self::UPDATE,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                // TODO: can we somehow bypass intermediate if we have a new update upcoming - we probably can't because this would probably cause memory leaks and other funny things that we don't like
                // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                // as it, itself is protected by other atomics, so we can use `Relaxed`
                let intermediate = self.intermediate_ptr.load(Ordering::Relaxed);
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
                    unsafe {
                        D::from(Self::strip_metadata(prev));
                    }
                } else {
                    // unset the update flag
                    self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                }
                // unset the `weak` update flag from the intermediate ref cnt
                self.intermediate_ref_cnt
                    .fetch_and(!Self::OTHER_UPDATE, Ordering::AcqRel);
                true
            }
            _ => false,
        }
    }

    #[cold]
    fn try_update_intermediate(&self) {
        match self.intermediate_ref_cnt.compare_exchange(
            0,
            Self::UPDATE | Self::OTHER_UPDATE,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                // take the update
                let update = self.updated.swap(null_mut(), Ordering::AcqRel); // TODO: can this be weaker?
                // check if we even have an update
                if !update.is_null() {
                    // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                    // as it, itself is protected by other atomics, so we can use `Relaxed`
                    let metadata =
                        Self::get_metadata(self.intermediate_ptr.load(Ordering::Relaxed));
                    let update = Self::merge_ptr_and_metadata(update, metadata).cast_mut();
                    // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                    // as it, itself is protected by other atomics, but when performing a load we have be
                    // able to ensure that we never observe the update of `intermediate_ptr`
                    // before the update of `intermediate_ref_cnt` in order to make the protection
                    // of it by the other atomics effective, so we have to use `Release`
                    self.intermediate_ptr.store(update, Ordering::Release);
                    // unset the update flag
                    self.intermediate_ref_cnt
                        .fetch_and(!Self::UPDATE, Ordering::AcqRel);
                    // try finishing the update up
                    let mut curr = 0;
                    loop {
                        match self.curr_ref_cnt.compare_exchange(
                            curr,
                            Self::UPDATE,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => {
                                // `ptr` doesn't have to care about anything other than itself
                                // as it, itself is protected by other atomics, so we can use `Relaxed`
                                let prev = self.ptr.load(Ordering::Relaxed);
                                self.ptr.store(update, Ordering::Relaxed);
                                // unset the update flag
                                self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                                // unset the `weak` update flag from the intermediate ref cnt
                                self.intermediate_ref_cnt
                                    .fetch_and(!Self::OTHER_UPDATE, Ordering::AcqRel); // TODO: acquire should suffice here!
                                // drop the `virtual reference` we hold to the `D`

                                // SAFETY: we know that we hold a `virtual reference` to the `D`
                                // which `prev` points to and thus we are allowed to drop
                                // this `virtual reference`, once we replace `self.ptr` (or in other
                                // word `prev`)
                                unsafe {
                                    D::from(Self::strip_metadata(prev));
                                }
                                break;
                            }
                            Err(_) => {
                                // set the `OTHER_UPDATE` flag to indicate that there is an update pending
                                // and no other explicit update to `ptr` may occur until this updates has been
                                // implicitly updated (but only if such a flag isn't present yet)

                                // ORDERING: the failure ordering of `Relaxed` can't race because we are performing an `Acquire`
                                // ordered atomic operation right here.
                                if self
                                    .curr_ref_cnt
                                    .fetch_or(Self::OTHER_UPDATE, Ordering::AcqRel)
                                    == 0
                                {
                                    curr = Self::OTHER_UPDATE;
                                    // retry update, as no implicit update can occur anymore
                                    continue;
                                }
                                break;
                            }
                        }
                    }
                } else {
                    // unset the update flags as there's no update to be applied here
                    self.intermediate_ref_cnt
                        .fetch_and(!(Self::UPDATE | Self::OTHER_UPDATE), Ordering::AcqRel);
                }
            }
            Err(_) => {}
        }
    }

    /// Update the value inside the SwapArc to value passed in `updated`.
    #[inline]
    pub fn store(&self, updated: D) {
        // SAFETY: we know that `updated` is an instance of `D`, so we can generate a valid ptr to its content.
        unsafe {
            self._store_raw(updated.into_ptr());
        }
    }

    /// SAFETY: `updated` has to be a pointer that points to a valid instance
    /// of `T` and has to be acquired by calling `D::as_ptr` or via similar means.
    #[cfg(feature = "ptr-ops")]
    #[inline]
    pub unsafe fn store_raw(&self, updated: *const T) {
        let updated = Self::strip_metadata(updated);
        // SAFETY: this is safe because the caller promises that `updated` is valid.
        let tmp = ManuallyDrop::new(D::from(updated.cast_mut()));
        tmp.increase_ref_cnt();
        self._store_raw(updated);
    }

    /// SAFETY: `updated` has to be a pointer that points to a valid instance
    /// of `T` and has to be acquired by calling `D::as_ptr` or via similar means.
    unsafe fn _store_raw(&self, updated: *const T) {
        let new = updated.cast_mut();
        let backoff = Backoff::new();
        loop {
            // signal that `intermediate_ptr` is now being updated and may not be relied upon
            // via the `UPDATE` flag.
            // additionally we set the `OTHER_UPDATE` flag in advance to signal that the
            // "new" value inside `intermediate_ptr` hasn't propagated to `ptr` yet.
            match self.intermediate_ref_cnt.compare_exchange(
                0,
                Self::UPDATE | Self::OTHER_UPDATE,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // clear out old updates to make sure our update won't be overwritten by them in the future
                    let old = self.updated.swap(null_mut(), Ordering::AcqRel); // TODO: can this be weaker?
                    // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
                    // as it, itself is protected by other atomics, so we can use `Relaxed`
                    let metadata =
                        Self::get_metadata(self.intermediate_ptr.load(Ordering::Relaxed));
                    let new = Self::merge_ptr_and_metadata(new, metadata).cast_mut();
                    self.intermediate_ptr.store(new, Ordering::Relaxed);
                    // unset the update flag to signal that `intermediate_ptr` may now be relied upon
                    self.intermediate_ref_cnt
                        .fetch_and(!Self::UPDATE, Ordering::AcqRel);
                    if !old.is_null() {
                        // drop the `virtual reference` we hold to the `D`

                        // SAFETY: we know that we hold a `virtual reference` to the `D`
                        // which `updated` pointed to before but we took it out of
                        // `updated` and we know that the value in `updated` has to be valid
                        // while it is in there.
                        D::from(old);
                    }
                    // try finishing the update up
                    let mut curr = 0;
                    loop {
                        // try setting the `UPDATE` flag to signal that `ptr` is being updated
                        // and may not be relied upon.
                        match self.curr_ref_cnt.compare_exchange(
                            curr,
                            Self::UPDATE,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => {
                                // `ptr` doesn't have to care about anything other than itself
                                // as it, itself is protected by other atomics, so we can use `Relaxed`
                                let prev = self.ptr.load(Ordering::Relaxed);
                                self.ptr.store(new, Ordering::Relaxed);
                                // unset the update flag to signal that `ptr` may now be used again
                                self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                                // unset the `weak` update flag to signal that new updates to
                                // `intermediate_ptr` are now permitted again
                                self.intermediate_ref_cnt
                                    .fetch_and(!Self::OTHER_UPDATE, Ordering::AcqRel);
                                // drop the `virtual reference` we hold to the `D`

                                // SAFETY: we know that we hold a `virtual reference` to the `D`
                                // which `prev` points to and thus we are allowed to drop
                                // this `virtual reference`, once we replace `self.ptr` (or in other
                                // word `prev`)
                                D::from(Self::strip_metadata(prev));
                                break;
                            }
                            Err(_) => {
                                // set the `OTHER_UPDATE` flag to indicate that there is an update pending
                                // and no other explicit update to `ptr` may occur until this update has been
                                // implicitly updated (but only if such a flag isn't present yet)

                                // ORDERING: the failure ordering of `Relaxed` can't race because we are performing an `Acquire`
                                // ordered atomic operation right here.
                                if self
                                    .curr_ref_cnt
                                    .fetch_or(Self::OTHER_UPDATE, Ordering::AcqRel)
                                    == 0
                                {
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
                    // note: we don't need to add a counter to determine which update is the most recent as that would be
                    // impossible anyways as there is no concept of time among two threads as long as they aren't synchronized
                    // which means there's no way for us to know which update is the most recent in the first place
                    let old = self.updated.swap(new, Ordering::AcqRel);
                    if !old.is_null() {
                        // drop the `virtual reference` we hold to the `D`

                        // SAFETY: we know that we hold a `virtual reference` to the `D`
                        // which `updated` pointed to before but we took it out of
                        // `updated` and we know that the value in `updated` has to be valid
                        // while it is in there.
                        D::from(old);
                    }
                    break;
                }
            }
        }
    }

    // TODO: maybe add some method along the lines of:
    // unsafe fn try_compare_exchange<const IGNORE_META: bool>(&self, old: *const T, new: D) -> Result<bool, D>

    /// Note: this can cause "deadlocks" if there are any other (loaded) references alive.
    /// SAFETY: `old` has to be a pointer that was acquired through
    /// methods of this `SwapArc` instance. `old` may contain
    /// metadata.
    /// `new` has to be a pointer to a valid instance of `D`.
    #[cfg(feature = "ptr-ops")]
    pub unsafe fn try_compare_exchange_with_meta(&self, old: *const T, new: *const T) -> bool {
        self.compare_exchange_inner::<false>(old, new)
    }

    /// Note: this can cause "deadlocks" if there are any other (loaded) references alive.
    /// SAFETY: `old` has to be a pointer that was acquired through
    /// methods of this `SwapArc` instance. `old` may contain
    /// metadata. The metadata of `old` and the current pointer
    /// is ignored, i.e it doesn't have to match.
    /// `new` has to be a pointer to a valid instance of `D`.
    #[cfg(feature = "ptr-ops")]
    pub unsafe fn try_compare_exchange_ignore_meta(&self, old: *const T, new: *const T) -> bool {
        self.compare_exchange_inner::<true>(old, new)
    }

    unsafe fn compare_exchange_inner<const IGNORE_META: bool>(&self, old: *const T, new: *const T) -> bool {
        use crate::ptr::map_addr;

        let old = if IGNORE_META {
            map_addr(old, |x| x & !Self::META_MASK)
        } else {
            old
        };
        let backoff = Backoff::new();
        while !self
            .intermediate_ref_cnt
            .compare_exchange_weak(
                0,
                Self::UPDATE | Self::OTHER_UPDATE,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            println!("waiting...");
            // back-off
            backoff.snooze();
        }
        // This load may be `Relaxed` because it is guarded by the `intermediate_ref_cnt`.
        let intermediate = self.intermediate_ptr.load(Ordering::Relaxed).cast_const();
        let intermediate = if IGNORE_META {
            map_addr(intermediate, |x| x & !Self::META_MASK)
        } else {
            intermediate
        };
        if intermediate != old {
            // ORDERING: We use Acquire here because we have a Release above with which we can establish
            // a happens-before relationship with the RMW (compare_exchange) above
            self.intermediate_ref_cnt.fetch_and(
                !(Self::UPDATE | Self::OTHER_UPDATE),
                // ORDERING: This is `Acquire` because we only need to mak sure future writes happen
                // after this and we don't have to care about anything that happened before this
                // (as the only important thing happening in between these two modifications of
                // `intermediate_ref_cnt` is the load of `intermediate_ptr` which is sequenced-before this)
                Ordering::Acquire,
            );
            return false;
        }
        // SAFETY: This is safe because we know that `new` points to a valid instance of `D`.
        let tmp = ManuallyDrop::new(D::from(Self::strip_metadata(new)));
        tmp.increase_ref_cnt();

        // clear out old updates to make sure our update won't be overwritten by them in the future
        let old_update = self.updated.swap(null_mut(), Ordering::AcqRel); // TODO: can this be weaker?

        self.intermediate_ptr
            .store(new.cast_mut(), Ordering::Relaxed);
        // unset the update flag
        self.intermediate_ref_cnt
            .fetch_and(!Self::UPDATE, Ordering::AcqRel);
        if !old_update.is_null() {
            // drop the `virtual reference` we hold to the `D`

            // SAFETY: we know that we hold a `virtual reference` to the `D`
            // which `updated` pointed to before but we took it out of
            // `updated` and we know that the value in `updated` has to be valid
            // while it is in there.
            D::from(old_update);
        }
        let mut curr = 0;
        loop {
            match self.curr_ref_cnt.compare_exchange(
                curr,
                Self::UPDATE,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // ORDERING: Relaxed should be okay because curr_ref_cnt guards `ptr`
                    // and ensures that other threads see the update because
                    // of the happens-before-relationship between
                    let prev = self.ptr.load(Ordering::Relaxed);
                    self.ptr.store(new.cast_mut(), Ordering::Relaxed);
                    // unset the update flag
                    self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::AcqRel);
                    // unset the `weak` update flag from the intermediate ref cnt
                    self.intermediate_ref_cnt
                        .fetch_and(!Self::OTHER_UPDATE, Ordering::AcqRel);
                    // drop the `virtual reference` we hold to the `D`

                    // SAFETY: we know that we hold a `virtual reference` to the `D`
                    // which `prev` points to and thus we are allowed to drop
                    // this `virtual reference`, once we replace `self.ptr` (or in other
                    // word `prev`)
                    D::from(Self::strip_metadata(prev));
                    break;
                }
                Err(_) => {
                    // signal that it's allowed to implicitly update this now
                    if self
                        .curr_ref_cnt
                        .fetch_or(Self::OTHER_UPDATE, Ordering::AcqRel)
                        == 0
                    {
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

            // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
            // as it, itself is protected by other atomics, so we can use `Relaxed`
            match self.intermediate_ptr.compare_exchange_weak(
                curr,
                ptr::map_addr(curr, |x| (x & !Self::META_MASK) | prefix).cast_mut(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(err) => {
                    if is_cmp_exchg_really_weak() && curr == err {
                        // the fail was spurious
                        continue;
                    }
                    curr = err;
                }
            }

            backoff.spin(); // TODO: should we really backoff here? the other thread will make progress anyways and we will only have to spin once more if it makes progress again
        }
    }

    /// Tries to replace the current metadata with the new one if there was no update in between.
    /// `old` is the previous pointer and should contain the previous metadata.
    /// `metadata` is the new metadata, the old one will be replaced with.
    #[cfg(feature = "ptr-ops")]
    pub fn try_update_meta(&self, old: *const T, metadata: usize) -> bool {
        let prefix = metadata & Self::META_MASK;

        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        self.intermediate_ptr
            .compare_exchange(
                old.cast_mut(),
                ptr::map_addr(old, |x| (x & !Self::META_MASK) | prefix).cast_mut(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Sets all bits of the internal pointer which are set in the `inactive_bits` parameter
    pub fn set_in_metadata(&self, active_bits: usize) {
        let backoff = Backoff::new();
        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        let mut curr = self.intermediate_ptr.load(Ordering::Relaxed);
        loop {
            let prefix = active_bits & Self::META_MASK;

            // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
            // as it, itself is protected by other atomics, so we can use `Relaxed`
            match self.intermediate_ptr.compare_exchange_weak(
                curr,
                ptr::map_addr_mut(curr, |x| x | prefix),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(err) => {
                    if is_cmp_exchg_really_weak() && curr == err {
                        // the fail was spurious
                        continue;
                    }
                    curr = err;
                }
            }
            backoff.spin(); // TODO: should we really backoff here? the other thread will make progress anyways and we will only have to spin once more if it makes progress again
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

            // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
            // as it, itself is protected by other atomics, so we can use `Relaxed`
            match self.intermediate_ptr.compare_exchange_weak(
                curr,
                ptr::map_addr_mut(curr, |x| x & !prefix),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(err) => {
                    if is_cmp_exchg_really_weak() && err == curr {
                        // the fail was spurious
                        continue;
                    }
                    curr = err;
                }
            }
            backoff.spin(); // TODO: should we really backoff here? the other thread will make progress anyways and we will only have to spin once more if it makes progress again
        }
    }

    /// Returns the metadata stored inside the internal pointer
    pub fn load_metadata(&self) -> usize {
        // ORDERING: `intermediate_ptr` doesn't have to care about anything other than itself
        // as it, itself is protected by other atomics, so we can use `Relaxed`
        ptr::expose_addr(self.intermediate_ptr.load(Ordering::Relaxed)) & Self::META_MASK
    }

    #[inline(always)]
    fn get_metadata(ptr: *const T) -> usize {
        ptr::expose_addr(ptr) & Self::META_MASK
    }

    #[inline(always)]
    fn strip_metadata(ptr: *const T) -> *const T {
        ptr::map_addr(ptr, |x| x & !Self::META_MASK)
    }

    #[inline(always)]
    fn merge_ptr_and_metadata(ptr: *const T, metadata: usize) -> *const T {
        ptr::map_addr(ptr, |x| x | metadata)
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

    // TODO: MAYBE: introduce FORCE_UPDATing into the data structure in order for the user to be able
    // TODO: to ensure that there's NO update pending
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop
    for SwapArcAnyMeta<T, D, METADATA_BITS>
{
    fn drop(&mut self) {
        let updated = *self.updated.get_mut();
        if !updated.is_null() {
            // SAFETY: we know that we hold a `virtual reference` to the `D`
            // which `updated` points to and thus we are allowed to drop
            // this `virtual reference`, once we drop the `SwapArc`.
            unsafe {
                D::from(updated);
            }
        }
        let curr = Self::strip_metadata(*self.ptr.get_mut());
        let intermediate = Self::strip_metadata(*self.intermediate_ptr.get_mut());
        if intermediate != curr {
            // we have to handle intermediate here too as it is possible for it to be non-null
            // on drop of `SwapArc` if there's an update to `curr` pending and there are no instances of
            // an value loaded through `intermediate` and a `store` happens meaning that once all `curr`
            // instances get dropped, `intermediate` will be non-null.
            unsafe {
                D::from(intermediate);
            }
        }
        // drop the current `D`

        // SAFETY: we know that we hold a `virtual reference` to the `D`
        // which `curr` points to and thus we are allowed to drop
        // this `virtual reference`, once we drop the `SwapArc`.
        unsafe {
            D::from(curr);
        }
    }
}

cfg_if! {
    if #[cfg(feature = "ptr-ops")] {
        pub struct SwapArcPtrGuard<'a, T: Send + Sync, D: DataPtrConvert<T> = Arc<T>, const METADATA_BITS: u32 = 0> {
            parent: &'a LocalData<T, D, METADATA_BITS>,
            ptr: *const T,
            gen_cnt: usize,
        }

        impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> SwapArcPtrGuard<'_, T, D, METADATA_BITS> {

            #[inline(always)]
            pub fn as_raw(&self) -> *const T {
                self.ptr
            }

        }

        impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Clone for SwapArcPtrGuard<'_, T, D, METADATA_BITS> {
            fn clone(&self) -> Self {
                self.parent.parent().load_raw()
            }
        }

        impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop for SwapArcPtrGuard<'_, T, D, METADATA_BITS> {
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
                fn slow_drop<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32>(parent: &LocalData<T, D, METADATA_BITS>, data: &mut LocalDataInner<T, D>, gen_cnt: usize) {
                    if gen_cnt == data.new.gen_cnt {
                        data.new.ref_cnt -= 1;
                        if data.new.ref_cnt == 0 {
                            if data.intermediate.ref_cnt != 0 {
                                // increase the ref count of the new value
                                data.intermediate.val().increase_ref_cnt();
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

        impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Debug for SwapArcPtrGuard<'_, T, D, METADATA_BITS> {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                let tmp = format!("{:?}", self.ptr);
                f.write_str(tmp.as_str())
            }
        }

        pub struct SwapArcFullPtrGuard<T: Send + Sync, D: DataPtrConvert<T> = Arc<T>, const METADATA_BITS: u32 = 0> {
            inner: D,
            ptr: *const T,
        }

        impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> SwapArcFullPtrGuard<T, D, METADATA_BITS> {

            #[inline(always)]
            pub fn as_raw(&self) -> *const T {
                self.ptr
            }

        }

        impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Clone for SwapArcFullPtrGuard<T, D, METADATA_BITS> {
            fn clone(&self) -> Self {
                Self {
                    inner: self.inner.clone(),
                    ptr: self.ptr,
                }
            }
        }

        impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Debug for SwapArcFullPtrGuard<T, D, METADATA_BITS> {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                let tmp = format!("{:?}", self.ptr);
                f.write_str(tmp.as_str())
            }
        }
    }
}

pub struct SwapArcGuard<'a, T: Send + Sync, D: DataPtrConvert<T> = Arc<T>, const METADATA_BITS: u32 = 0> {
    parent: &'a LocalData<T, D, METADATA_BITS>,
    fake_ref: ManuallyDrop<D>,
    gen_cnt: usize,
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop
    for SwapArcGuard<'_, T, D, METADATA_BITS>
{
    #[inline]
    fn drop(&mut self) {
        // SAFETY: This is safe because we know that we are the only thread that
        // is able to access the thread local data at this time and said data has to be initialized
        // and we also know, that the pointer has to be non-null
        let data = unsafe { self.parent.inner.get().as_mut().unwrap_unchecked() };
        // release the reference we hold
        if likely(self.gen_cnt == data.curr.gen_cnt) {
            // we have a reference to the locally owned `D` so we
            // don't have the potential to interact with outside
            // counters (counters that aren't thread local).
            // this is the fast path, as as long as no updates
            // occur we will always get a reference to the
            // locally owned `D`.
            data.curr.ref_cnt -= 1;
        } else {
            slow_drop(self.parent, data, self.gen_cnt);
        }
        #[cold]
        fn slow_drop<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32>(
            parent: &LocalData<T, D, METADATA_BITS>,
            data: &mut LocalDataInner<T, D>,
            gen_cnt: usize,
        ) {
            if gen_cnt == data.new.gen_cnt {
                data.new.ref_cnt -= 1;
                if data.new.ref_cnt == 0 {
                    if data.intermediate.ref_cnt != 0 {
                        // increase the ref count of the new value
                        data.intermediate.val().increase_ref_cnt();
                        // SAFETY: we know that the reference count for the stored `D` got
                        // increased by us and thus we can decrease it again when
                        // it isn't needed anymore.
                        data.new = unsafe { mem::take(&mut data.intermediate).make_drop() };
                        parent
                            .parent()
                            .intermediate_ref_cnt
                            .fetch_sub(1, Ordering::AcqRel);
                    }
                }
            } else {
                data.intermediate.ref_cnt -= 1;
                if data.intermediate.ref_cnt == 0 {
                    // we have no local references to the `intermediate` value anymore
                    // so release the outside reference as well.
                    parent
                        .parent()
                        .intermediate_ref_cnt
                        .fetch_sub(1, Ordering::AcqRel);
                }
            }
        }
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Deref
    for SwapArcGuard<'_, T, D, METADATA_BITS>
{
    type Target = D;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.fake_ref.deref()
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Borrow<D>
    for SwapArcGuard<'_, T, D, METADATA_BITS>
{
    #[inline]
    fn borrow(&self) -> &D {
        self.fake_ref.deref()
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> AsRef<D>
    for SwapArcGuard<'_, T, D, METADATA_BITS>
{
    #[inline]
    fn as_ref(&self) -> &D {
        self.fake_ref.deref()
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T> + Display, const METADATA_BITS: u32> Display
    for SwapArcGuard<'_, T, D, METADATA_BITS>
{
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        D::fmt(self.as_ref(), f)
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T> + Debug, const METADATA_BITS: u32> Debug
    for SwapArcGuard<'_, T, D, METADATA_BITS>
{
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        D::fmt(self.as_ref(), f)
    }
}

struct SwapArcStrongGuard<'a, T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> {
    parent: &'a SwapArcAnyMeta<T, D, METADATA_BITS>,
    fake_ref: ManuallyDrop<D>,
    ref_src: RefSource,
    _no_send_guard: PhantomData<RefCell<()>>, // this ensures that this struct is not sent over to other threads
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Drop
    for SwapArcStrongGuard<'_, T, D, METADATA_BITS>
{
    fn drop(&mut self) {
        // release the reference we hold
        match self.ref_src {
            RefSource::Curr => {
                let ref_cnt = self.parent.curr_ref_cnt.fetch_sub(1, Ordering::AcqRel);
                // Note: we only perform implicit updates when `OTHER_UPDATE` is set and there
                // are no other references alive as `OTHER_UPDATE` signals that there is still
                // an update pending and it would be useless to try to perform an implicit
                // update if no update is pending.
                if ref_cnt == SwapArcAnyMeta::<T, D, METADATA_BITS>::OTHER_UPDATE + 1 {
                    self.parent.try_update_curr();
                }
            }
            RefSource::Intermediate => {
                let ref_cnt = self
                    .parent
                    .intermediate_ref_cnt
                    .fetch_sub(1, Ordering::AcqRel);
                // fast-rejection path to ensure we are only trying to update if it's worth it:
                // first check if there are no other references alive, then check if there are
                // still updates pending.
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

struct LocalData<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> {
    parent: *const SwapArcAnyMeta<T, D, METADATA_BITS>, // this acts as a reference with hidden lifetime that only we know is safe because
    // `parent` won't be used in the drop impl and `LocalData` can only be accessed
    // through `parent`
    inner: UnsafeCell<LocalDataInner<T, D>>,
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32>
    LocalData<T, D, METADATA_BITS>
{
    #[inline]
    fn parent(&self) -> &SwapArcAnyMeta<T, D, METADATA_BITS> {
        // SAFETY: we know that `parent` has to be alive at this point
        // because otherwise `LocalData` wouldn't be alive.
        unsafe { self.parent.as_ref().unwrap_unchecked() }
    }
}

// SAFETY: this is safe because `parent` has to be alive when `LocalData` gets used
// as it is only reachable through a valid reference to `SwapArc`
unsafe impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_BITS: u32> Send
    for LocalData<T, D, METADATA_BITS>
{
}

struct LocalDataInner<T: Send + Sync, D: DataPtrConvert<T>> {
    next_gen_cnt: usize,
    intermediate: LocalCounted<T, D>,
    new: LocalCounted<T, D, true>,
    curr: LocalCounted<T, D, true>,
}

struct LocalCounted<T: Send + Sync, D: DataPtrConvert<T>, const DROP: bool = false> {
    gen_cnt: usize, // TODO: would it help somehow (the performance) if we were to make `gen_cnt` an `u8`? - it probably wouldn't!
    ptr: *mut T,
    ref_cnt: usize,
    _phantom_data: PhantomData<D>,
}

// SAFETY: These impls are safe as the lack of Send and Sync impls on ptrs is just due to
// historic reasons and doesn't have a logical foundation.
unsafe impl<T: Send + Sync, D: DataPtrConvert<T>> Send for LocalCounted<T, D> {}
unsafe impl<T: Send + Sync, D: DataPtrConvert<T>> Sync for LocalCounted<T, D> {}

impl<T: Send + Sync, D: DataPtrConvert<T>, const DROP: bool> LocalCounted<T, D, DROP> {
    /// SAFETY: The caller has to ensure that the safety invariants relied upon
    /// in the `val`, `refill_unchecked` and `drop` methods are valid.
    unsafe fn new(parent: &mut LocalDataInner<T, D>, ptr: *const T) -> Self {
        let gen_cnt = parent.next_gen_cnt;
        let res = parent.next_gen_cnt.overflowing_add(1);
        // if an overflow occurs, we add an additional 1 to the result in order to never
        // reach 0 which is reserved for the "default" `gen_cnt`
        parent.next_gen_cnt = res.0 + res.1 as usize;
        Self {
            gen_cnt,
            ptr: ptr.cast_mut(),
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
                unsafe {
                    D::from(self.ptr);
                }
            }
        }
        self.ptr = ptr.cast_mut();
        self.ref_cnt = 1;
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T>> LocalCounted<T, D, false> {
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

impl<T: Send + Sync, D: DataPtrConvert<T>, const DROP: bool> Default for LocalCounted<T, D, DROP> {
    #[inline]
    fn default() -> Self {
        Self {
            gen_cnt: 0,
            ptr: null_mut(),
            ref_cnt: 0,
            _phantom_data: Default::default(),
        }
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const DROP: bool> Drop for LocalCounted<T, D, DROP> {
    #[inline]
    fn drop(&mut self) {
        if DROP {
            if !self.ptr.is_null() {
                // SAFETY: the person defining this struct has to make sure that
                // choosing `DROP` is correct.
                unsafe {
                    D::from(self.ptr);
                }
            }
        }
    }
}

/// SAFETY: Types implementing this trait are expected to perform
/// reference counting through cloning/dropping internally.
/// To be precise, the `RefCnt` is expected to increment
/// the reference count on `clone` calls, and decrement
/// it on `drop`.
/// NOTE: `Drop` is not required here as types such as
/// `Option<Arc<T>>` fulfill all the requirements without
/// needing special drop glue.
pub unsafe trait RefCnt: Send + Sync + Clone {}

pub trait DataPtrConvert<T: Send + Sync>: RefCnt + Sized {
    /// This method may not alter the reference count of the
    /// reference counted "object".
    ///
    /// SAFETY: `ptr` has to point to a valid instance of `T`
    /// and has to have been acquired by calling `DataPtrConvert::into`
    /// furthermore the `DataPtrConvert` which this was called on
    /// may never have reached a reference count of `0`.
    unsafe fn from(ptr: *const T) -> Self;

    /// This method may not alter the reference count of the
    /// reference counted "object".
    fn into_ptr(self) -> *const T;

    /// This method increments the reference count of the
    /// reference counted "object" directly.
    #[inline]
    fn increase_ref_cnt(&self) {
        mem::forget(self.clone());
    }
}

// SAFETY: This is safe because `Arc<T>` will increment an
// internal reference count on `clone` calls and decrement
// it on drop.
unsafe impl<T: Send + Sync> RefCnt for Arc<T> {}

impl<T: Send + Sync> DataPtrConvert<T> for Arc<T> {
    #[inline]
    unsafe fn from(ptr: *const T) -> Self {
        // SAFETY: this is safe as the safety contract dictates
        // `ptr` to not have been freed and to be a valid pointer
        // that was acquired through `Self::into_ptr` which in this
        // case means `Arc::into_raw`
        Arc::from_raw(ptr)
    }

    #[inline]
    fn into_ptr(self) -> *const T {
        Arc::into_raw(self)
    }
}

// SAFETY: This is safe because on `drop` and `clone`, `Option<Arc<T>>`
// will either: in case of `None`, "do nothing"
// or in case of `Some` do the same as `Arc<T>`
// which as explained above is also allowed to implement
// `RefCnt`.
unsafe impl<T: Send + Sync> RefCnt for Option<Arc<T>> {}

impl<T: Send + Sync> DataPtrConvert<T> for Option<Arc<T>> {
    #[inline]
    unsafe fn from(ptr: *const T) -> Self {
        if !ptr.is_null() {
            // SAFETY: this is safe because this is doing the exact same as
            // the `from` method in the `DataPtrConvert<T>` implementation of
            // `Arc<T>`.
            Some(Arc::from_raw(ptr))
        } else {
            None
        }
    }

    #[inline]
    fn into_ptr(self) -> *const T {
        match self {
            None => null(),
            Some(val) => Arc::into_raw(val),
        }
    }
}

/// This function indicates whether the current target does really
/// have a weak compare exchange method.
/// NOTE: It **Must not** be relied upon for correctness.
#[inline(always)]
const fn is_cmp_exchg_really_weak() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    return false;
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    return true;
}

mod ptr {

    // TODO: switch to strict_provenance once it has been stabilized

    #[inline]
    pub(crate) fn map_addr<T>(ptr: *const T, f: impl FnOnce(usize) -> usize) -> *const T {
        f(expose_addr(ptr)) as *const T
    }

    #[inline]
    pub(crate) fn map_addr_mut<T>(ptr: *mut T, f: impl FnOnce(usize) -> usize) -> *mut T {
        f(expose_addr(ptr)) as *mut T
    }

    #[inline]
    pub(crate) fn expose_addr<T>(ptr: *const T) -> usize {
        ptr as usize
    }

    #[inline]
    pub(crate) const fn from_exposed_addr<T>(exposed_addr: usize) -> *const T {
        exposed_addr as *const T
    }

    #[inline]
    pub(crate) const fn from_exposed_addr_mut<T>(exposed_addr: usize) -> *mut T {
        exposed_addr as *mut T
    }
}

#[cfg(all(test, not(miri)))]
#[test]
fn test_load_multi() {
    use std::hint::black_box;
    use std::thread;
    let tmp: Arc<SwapArcAnyMeta<i32, Arc<i32>, 0>> = Arc::new(SwapArcAnyMeta::new(Arc::new(3)));
    let mut threads = vec![];
    for _ in 0..20 {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..2000 {
                let l1 = tmp.load();
                black_box(l1);
            }
        }));
    }
    for _ in 0..20 {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..2000 {
                tmp.store(Arc::new(rand::random()));
            }
        }));
    }
    threads
        .into_iter()
        .for_each(|thread| thread.join().unwrap());
}

#[cfg(all(test, miri))]
#[test]
fn test_load_multi_miri() {
    use std::hint::black_box;
    use std::thread;
    let tmp: Arc<SwapArcAnyMeta<i32, Arc<i32>, 0>> = Arc::new(SwapArcAnyMeta::new(Arc::new(3)));
    let mut threads = vec![];
    for _ in 0..4 {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..200 {
                let l1 = tmp.load();
                black_box(l1);
            }
        }));
    }
    for _ in 0..4 {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..200 {
                tmp.store(Arc::new(rand::random()));
            }
        }));
    }
    threads
        .into_iter()
        .for_each(|thread| thread.join().unwrap());
}

#[cfg(all(test, miri))]
#[test]
fn test_other_load_multi_miri() {
    use std::hint::black_box;
    use std::thread;
    let tmp: Arc<SwapArcAnyMeta<i32, Arc<i32>, 0>> = Arc::new(SwapArcAnyMeta::new(Arc::new(3)));
    let mut threads = vec![];
    for _ in 0..4 {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..200 {
                let l1 = tmp.load();
                black_box(l1);
            }
        }));
    }
    for _ in 0..1 {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..20 {
                tmp.store(Arc::new(rand::random()));
            }
        }));
    }
    threads
        .into_iter()
        .for_each(|thread| thread.join().unwrap());
}

#[test]
fn test_load() {
    let tmp = SwapArc::new(Arc::new(3));
    tmp.load();
}

#[test]
fn test_store() {
    let tmp = SwapArc::new(Arc::new(3));
    tmp.store(Arc::new(-2));
}
