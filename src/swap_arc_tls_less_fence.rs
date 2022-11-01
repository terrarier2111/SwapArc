use std::marker::PhantomData;
use std::{mem, ptr};
use std::borrow::Borrow;
use std::cell::UnsafeCell;
use std::fmt::{Debug, Display, Formatter};
use std::intrinsics::unlikely;
use std::mem::{align_of, ManuallyDrop, MaybeUninit};
use std::ops::Deref;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};
use thread_local::ThreadLocal;

#[derive(Copy, Clone, Debug)]
pub enum UpdateResult {
    Ok,
    AlreadyUpdating,
    NoUpdate,
}

// const IDLE_MARKER: usize = 1 << 0;

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

const fn assert_alignment<T, const METADATA_BITS: u32>() -> bool {
    let free_bits = most_sig_set_bit(align_of::<T>()).unwrap_or(0);
    if free_bits < METADATA_BITS + 1 {
        unreachable!("The alignment of T is insufficient, expected `{}`, but found `{}`", 1 << (METADATA_BITS + 1), 1 << free_bits);
    }
    true
}

// FIXME: note somewhere that this data structure requires T to at least be aligned to 2 bytes
// FIXME: and that the least significant bit of T's pointer is used internally, also add a static assertion for this!
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
/// (at most 2 additional updates).
pub struct SwapArcIntermediateTLS<T: Send + Sync, D: DataPtrConvert<T> = Arc<T>, const METADATA_HEADER_BITS: u32 = 0> {
    updated: AtomicPtr<T>,
    curr: AtomicPtr<T>,
    thread_local: ThreadLocal<LocalData<T, D, METADATA_HEADER_BITS>>,
    updating: Mutex<bool>,
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> SwapArcIntermediateTLS<T, D, METADATA_PREFIX_BITS> {

    pub fn new(val: D) -> Arc<Self> {
        static_assertions::const_assert!(assert_alignment());
        let val = ManuallyDrop::new(val);
        let virtual_ref = val.as_ptr();
        Arc::new(Self {
            updated: AtomicPtr::new(null_mut()),
            curr: AtomicPtr::new(virtual_ref.cast_mut()),
            thread_local: ThreadLocal::new(),
            updating: Mutex::new(false),
        })
    }

    /// SAFETY: this is only safe to call if the caller increments the
    /// reference count of the "object" `val` points to.
    fn dummy0() {}
    /*unsafe fn new_raw(val: *const T) -> Arc<Self> {
        Arc::new(Self {
            curr_ref_cnt: Default::default(),
            ptr: AtomicPtr::new(val.cast_mut()),
            intermediate_ref_cnt: Default::default(),
            intermediate_ptr: AtomicPtr::new(null_mut()),
            updated: AtomicPtr::new(null_mut()),
            thread_local: ThreadLocal::new(),
            _phantom_data: Default::default(),
        })
    }*/

    pub fn load<'a>(self: &'a Arc<Self>) -> SwapArcIntermediateGuard<'a, T, D, METADATA_PREFIX_BITS> {
        let mut new = false;
        let parent = self.thread_local.get_or(|| {
            new = true;
            /*LocalData {
                inner: MaybeUninit::new(self.clone().load_internal()),
                ref_cnt: 1,
            }*/
            let curr = self.curr.load(Ordering::Acquire);
            LocalData {
                parent: self.clone(),
                state: AtomicU8::new(STATE_UPDATED),
                inner: UnsafeCell::new(LocalDataInner {
                    new_val: Default::default(),
                    new_val_ptr: null_mut(),
                    new_ref_cnt: 0,
                    val: ManuallyDrop::new(D::from(curr)),
                    ref_cnt: 1,
                }),
            }
        });
        // SAFETY: This is safe because we know that we are the only thread that
        // is able to access the thread local data at this time and said data has to be initialized
        // and we also know, that the pointer has to be non-null
        let data = unsafe { parent.inner.get().as_mut().unwrap_unchecked() };
        if unlikely(new) {
            let fake_ref = ManuallyDrop::new(D::from(data.val.as_ptr()));
            return SwapArcIntermediateGuard {
                parent,
                fake_ref,
            };
        }
        data.ref_cnt += 1;
        if data.new_val_ptr.is_null() {
            if parent.state.compare_exchange(!STATE_IN_USE, Ordering::SeqCst).is_ok() {

            }
            let loaded =  // FIXME: try reducing this ordering!
                .map_addr(|x| x & !IDLE_MARKER);
            if !loaded.is_null() {
                data.new_val_ptr = loaded;
                data.new_val = ManuallyDrop::new(D::from(loaded));
            }
        } else {

        }
        // FIXME: try using intermediate value instead
        let fake_ref = ManuallyDrop::new(D::from(data.val.as_ptr()));
        SwapArcIntermediateGuard {
            parent,
            // SAFETY: we know that this is safe because the ref count is non-zero
            fake_ref,
        }
    }

    pub fn load_full(self: &Arc<Self>) -> D {
        self.load().as_ref().clone()
    }

    pub unsafe fn load_raw<'a>(self: &'a Arc<Self>) -> SwapArcIntermediatePtrGuard<'a, T, D, METADATA_PREFIX_BITS> {
        let guard = ManuallyDrop::new(self.load());
        SwapArcIntermediatePtrGuard {
            parent: guard.parent,
            ptr: guard.fake_ref.as_ptr(),
        }
    }

    /*
    fn try_update_curr(&self) -> bool {
        match self.curr_ref_cnt.compare_exchange(0, Self::UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => {
                // FIXME: can we somehow bypass intermediate if we have a new update upcoming - we probably can't because this would probably cause memory leaks and other funny things that we don't like
                let intermediate = self.intermediate_ptr.load(Ordering::SeqCst);
                // update the pointer
                let prev = self.ptr.load(Ordering::Acquire);
                if Self::strip_metadata(prev) != Self::strip_metadata(intermediate) {
                    self.ptr.store(intermediate, Ordering::Release);
                    // unset the update flag
                    self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                    println!("UPDATE status: {}", (self.intermediate_ref_cnt.load(Ordering::SeqCst) & Self::UPDATE != 0));
                    // unset the `weak` update flag from the intermediate ref cnt
                    self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::SeqCst); // FIXME: are we sure this can't happen if there is UPDATE set for intermediate_ref?
                    // drop the `virtual reference` we hold to the Arc
                    D::from(Self::strip_metadata(prev));
                } else {
                    // unset the update flag
                    self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                }
                true
            }
            _ => false,
        }
    }

    fn try_update_intermediate(&self) {
        match self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
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
                    match self.curr_ref_cnt.compare_exchange(0, Self::UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
                        Ok(_) => {
                            let prev = self.ptr.swap(update, Ordering::Release);
                            // unset the update flag
                            self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                            // unset the `weak` update flag from the intermediate ref cnt
                            self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::SeqCst);
                            // drop the `virtual reference` we hold to the Arc
                            D::from(Self::strip_metadata(prev));
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
    }*/

    fn try_update(&self, val: *const T) -> Option<bool> {
        let updating = self.updating.try_lock();
        if !updating.is_ok() {
            return None;
        }
        for local in self.thread_local.iter() {
            let curr = local.state.fetch_or(NORMALIZATION_MASK, Ordering::SeqCst);
            // the thread this `local` belongs to didn't already update and
            // it isn't idling
            if curr == STATE_IN_USE {
                // FIXME: fixup states for all thread_locals!
                return Some(false);
            }
        }
        self.curr.store(val.cast_mut(), Ordering::Release);
        for local in self.thread_local.iter() {
            local.state.store(STATE_UPDATABLE, Ordering::Release);
        }
        Some(true)
    }

    pub fn update(&self, updated: D) {
        updated.increase_ref_cnt();
        unsafe { self.update_raw(updated.into()); }
    }

    unsafe fn update_raw(&self, updated: *const T) {
        let updated = Self::strip_metadata(updated);
        loop {
            match self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    let new = updated.cast_mut();
                    let tmp = ManuallyDrop::new(D::from(new));
                    tmp.increase_ref_cnt();
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
                    match self.curr_ref_cnt.compare_exchange(0, Self::UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
                        Ok(_) => {
                            let prev = self.ptr.swap(new, Ordering::Release);
                            // unset the update flag
                            self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                            // unset the `weak` update flag from the intermediate ref cnt
                            self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::SeqCst);
                            // drop the `virtual reference` we hold to the Arc
                            D::from(Self::strip_metadata(prev));
                        }
                        Err(_) => {}
                    }
                    break;
                }
                Err(old) => {
                    if old & Self::UPDATE != 0 { // FIXME: what about Self::UPDATE_OTHER?
                        // somebody else already updates the current ptr, so we wait until they finish their update
                        continue;
                    }
                    // push our update up, so it will be applied in the future
                    let old = self.updated.swap(updated.cast_mut(), Ordering::SeqCst); // FIXME: should we add some sort of update counter
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

    unsafe fn try_compare_exchange<const IGNORE_META: bool>(&self, old: *const T, new: D/*&SwapArcIntermediateGuard<'_, T, D>*/) -> bool {
        if !self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            return false;
        }
        let intermediate = self.intermediate_ptr.load(Ordering::Acquire);
        let cmp_result = if IGNORE_META {
            Self::strip_metadata(intermediate) == old
        } else {
            intermediate.cast_const() == old
        };
        if !cmp_result {
            self.intermediate_ref_cnt.fetch_and(!(Self::UPDATE | Self::OTHER_UPDATE), Ordering::SeqCst);
            return false;
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
        match self.curr_ref_cnt.compare_exchange(0, Self::UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => {
                let prev = self.ptr.swap(new, Ordering::Release);
                // unset the update flag
                self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                // unset the `weak` update flag from the intermediate ref cnt
                self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::SeqCst);
                // drop the `virtual reference` we hold to the Arc
                D::from(Self::strip_metadata(prev));
            }
            Err(_) => {}
        }
        true
    }

    // FIXME: this causes "deadlocks" if there are any other references alive
    unsafe fn try_compare_exchange_with_meta(&self, old: *const T, new: *const T/*&SwapArcIntermediateGuard<'_, T, D>*/) -> bool {
        if !self.intermediate_ref_cnt.compare_exchange(0, Self::UPDATE | Self::OTHER_UPDATE, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            return false;
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
        tmp.increase_ref_cnt();
        self.intermediate_ptr.store(new.cast_mut(), Ordering::Release);
        // unset the update flag
        self.intermediate_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
        if !old_update.is_null() {
            // drop the `virtual reference` we hold to the Arc
            D::from(old_update);
        }
        match self.curr_ref_cnt.compare_exchange(0, Self::UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => {
                let prev = self.ptr.swap(new.cast_mut(), Ordering::Release);
                // unset the update flag
                self.curr_ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                // unset the `weak` update flag from the intermediate ref cnt
                self.intermediate_ref_cnt.fetch_and(!Self::OTHER_UPDATE, Ordering::SeqCst);
                // drop the `virtual reference` we hold to the Arc
                D::from(Self::strip_metadata(prev));
            }
            Err(_) => {}
        }
        true
    }

    pub fn update_metadata(&self, metadata: usize) {
        loop {
            let curr = self.intermediate_ptr.load(Ordering::Acquire);
            if self.try_update_meta(curr, metadata) { // FIXME: should this be a weak compare_exchange?
                break;
            }
        }
    }

    /// `old` should contain the previous metadata.
    pub fn try_update_meta(&self, old: *const T, metadata: usize) -> bool {
        let prefix = metadata & Self::META_MASK;
        self.intermediate_ptr.compare_exchange(old.cast_mut(), ptr::from_exposed_addr_mut(old.expose_addr() | prefix), Ordering::SeqCst, Ordering::SeqCst).is_ok()
    }

    pub fn set_in_metadata(&self, active_bits: usize) {
        self.intermediate_ptr.fetch_or(active_bits, Ordering::Release);
    }

    pub fn unset_in_metadata(&self, inactive_bits: usize) {
        self.intermediate_ptr.fetch_and(!inactive_bits, Ordering::Release);
    }

    pub fn load_metadata(&self) -> usize {
        self.intermediate_ptr.load(Ordering::Acquire).expose_addr() & Self::META_MASK
    }

    fn get_metadata(ptr: *const T) -> usize {
        ptr.expose_addr() & Self::META_MASK
    }

    fn strip_metadata(ptr: *const T) -> *const T {
        ptr::from_exposed_addr(ptr.expose_addr() & (!Self::META_MASK))
    }

    fn merge_ptr_and_metadata(ptr: *const T, metadata: usize) -> *const T {
        ptr::from_exposed_addr(ptr.expose_addr() | metadata)
    }

    const META_MASK: usize = {
        let mut result = 0;
        let mut i = 0;
        while METADATA_PREFIX_BITS > i {
            result |= 1 << i;
            i += 1;
        }
        result
    };

    /// This will force an update, this means that new
    /// readers will have to wait for all old readers to
    /// finish and to update the ptr, even when no update
    /// is queued this will block new readers for a short
    /// amount of time, until failure got detected
    fn dummy() {}
    /*fn force_update(&self) -> UpdateResult {
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

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> Drop for SwapArcIntermediateTLS<T, D, METADATA_PREFIX_BITS> {
    fn drop(&mut self) {
        // FIXME: how should we handle intermediate inside drop?
        let updated = self.updated.load(Ordering::Acquire);
        if !updated.is_null() {
            D::from(updated);
        }
        let curr = Self::strip_metadata(self.ptr.load(Ordering::Acquire));
        let intermediate = Self::strip_metadata(self.intermediate_ptr.load(Ordering::Acquire));
        if intermediate != curr {
            // FIXME: the reason why we have to do this currently is because the update function doesn't work properly, fix the root cause!
            D::from(intermediate);
        }
        // drop the current arc
        D::from(curr);
    }
}

/*
struct SwapArcIntermediateInternalGuard<T: Send + Sync, D: DataPtrConvert<T> = Arc<T>, const METADATA_PREFIX_BITS: u32 = 0> {
    parent: Arc<SwapArcIntermediateTLS<T, D, METADATA_PREFIX_BITS>>,
    fake_ref: ManuallyDrop<D>,
    ref_src: RefSource,
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> Drop for SwapArcIntermediateInternalGuard<T, D, METADATA_PREFIX_BITS> {
    fn drop(&mut self) {
        // release the reference we hold
        match self.ref_src {
            RefSource::Curr => {
                // let ref_cnt = self.parent.curr_ref_cnt.load(Ordering::SeqCst);
                let ref_cnt = self.parent.curr_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                if ref_cnt == 1 {
                    self.parent.try_update_curr();
                }
                // self.parent.curr_ref_cnt.fetch_sub(1, Ordering::SeqCst);
            }
            RefSource::Intermediate => {
                // FIXME: do we actually have to load the ref cnt before subtracting 1 from it?
                // let ref_cnt = self.parent.intermediate_ref_cnt.load(Ordering::SeqCst);
                let ref_cnt = self.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
                // fast-rejection path to ensure we are only trying to update if it's worth it
                // FIXME: this probably isn't correct: Note: UPDATE is set (seldom) on the immediate ref_cnt if there is a forced update waiting in the queue
                if (ref_cnt == 1/* || ref_cnt == SwapArcIntermediate::<T>::UPDATE*/) && !self.parent.updated.load(Ordering::Acquire).is_null() { // FIXME: does the updated check even help here?
                    self.parent.try_update_intermediate();
                }
                // self.parent.intermediate_ref_cnt.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T> + Display, const METADATA_PREFIX_BITS: u32> Display for SwapArcIntermediateInternalGuard<T, D, METADATA_PREFIX_BITS> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        D::fmt(self.fake_ref.deref(), f)
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T> + Debug, const METADATA_PREFIX_BITS: u32> Debug for SwapArcIntermediateInternalGuard<T, D, METADATA_PREFIX_BITS> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        D::fmt(self.fake_ref.deref(), f)
    }
}*/

pub struct SwapArcIntermediatePtrGuard<'a, T: Send + Sync, D: DataPtrConvert<T> = Arc<T>, const METADATA_PREFIX_BITS: u32 = 0> {
    parent: &'a LocalData<T, D, METADATA_PREFIX_BITS>,
    ptr: *const T,
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> SwapArcIntermediatePtrGuard<'_, T, D, METADATA_PREFIX_BITS> {

    #[inline]
    pub fn as_raw(&self) -> *const T {
        self.ptr
    }

}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> Clone for SwapArcIntermediatePtrGuard<'_, T, D, METADATA_PREFIX_BITS> {
    fn clone(&self) -> Self {
        // FIXME: use more recent thingy if available (do a new load)
        // SAFETY: This is safe because we know that we are the only thread that
        // is able to access the thread local data at this time and said data has to be initialized
        // and we also know, that the pointer has to be non-null
        unsafe { self.parent.inner.get().as_mut().unwrap_unchecked() }.ref_cnt += 1;
        SwapArcIntermediatePtrGuard {
            parent: self.parent,
            ptr: self.ptr,
        }
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> Drop for SwapArcIntermediatePtrGuard<'_, T, D, METADATA_PREFIX_BITS> {
    fn drop(&mut self) {
        // SAFETY: This is safe because we know that we are the only thread that
        // is able to access the thread local data at this time and said data has to be initialized
        // and we also know, that the pointer has to be non-null
        let data = unsafe { self.parent.inner.get().as_mut().unwrap_unchecked() };
        // release the reference we hold
        data.ref_cnt -= 1;
        if data.ref_cnt == 0 {
            // SAFETY: This is safe because we know that the reference count
            // was 1 before we just decremented it and thus we know that
            // `inner` has to be initialized right now.
            unsafe { data.inner.assume_init_drop(); }
        }
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> Debug for SwapArcIntermediatePtrGuard<'_, T, D, METADATA_PREFIX_BITS> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let tmp = format!("{:?}", self.ptr);
        f.write_str(tmp.as_str())
    }
}


pub struct SwapArcIntermediateGuard<'a, T: Send + Sync, D: DataPtrConvert<T> = Arc<T>, const METADATA_PREFIX_BITS: u32 = 0> {
    parent: &'a LocalData<T, D, METADATA_PREFIX_BITS>,
    fake_ref: ManuallyDrop<D>,
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> Drop for SwapArcIntermediateGuard<'_, T, D, METADATA_PREFIX_BITS> {
    fn drop(&mut self) {
        // SAFETY: This is safe because we know that we are the only thread that
        // is able to access the thread local data at this time and said data has to be initialized
        // and we also know, that the pointer has to be non-null
        let data = unsafe { self.parent.inner.get().as_mut().unwrap_unchecked() };
        // release the reference we hold
        if data.val.as_ptr() == self.fake_ref.as_ptr() {
            data.ref_cnt -= 1;
            if data.ref_cnt == 0 {
                // SAFETY: This is safe because we know that the reference count
                // was 1 before we just decremented it and thus we know that
                // `inner` has to be initialized right now.
                // unsafe { data.inner.assume_init_drop(); }

                if let Some(new) = data.new_val.take() {
                    let old_ref_ptr = data.val.as_ptr();
                    data.val = ManuallyDrop::new(new);
                    data.new_val_ptr = null();
                    data.ref_cnt = data.new_ref_cnt;
                    data.new_ref_cnt = 0;
                    self.parent.state.store(STATE_READY, Ordering::Release);
                    // FIXME: try update!
                    // data.parent.thread_local.iter()
                    // FIXME: go through all thread locals and check if
                    // signal that this thread has no pending updates
                }
            }
        } else {
            data.new_ref_cnt -= 1;
            if data.new_ref_cnt == 0 {
                // FIXME: is this really a NOOP?
            }
        }
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> Deref for SwapArcIntermediateGuard<'_, T, D, METADATA_PREFIX_BITS> {
    type Target = D;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.fake_ref.deref()
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> Borrow<D> for SwapArcIntermediateGuard<'_, T, D, METADATA_PREFIX_BITS> {
    #[inline]
    fn borrow(&self) -> &D {
        self.fake_ref.deref()
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T>, const METADATA_PREFIX_BITS: u32> AsRef<D> for SwapArcIntermediateGuard<'_, T, D, METADATA_PREFIX_BITS> {
    #[inline]
    fn as_ref(&self) -> &D {
        self.fake_ref.deref()
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T> + Display, const METADATA_PREFIX_BITS: u32> Display for SwapArcIntermediateGuard<'_, T, D, METADATA_PREFIX_BITS> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        D::fmt(self.as_ref(), f)
    }
}

impl<T: Send + Sync, D: DataPtrConvert<T> + Debug, const METADATA_PREFIX_BITS: u32> Debug for SwapArcIntermediateGuard<'_, T, D, METADATA_PREFIX_BITS> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        D::fmt(self.as_ref(), f)
    }
}

enum RefSource {
    Curr,
    Intermediate,
}

/*
const STATE_IDLING: u8 =  0b00; // 0
const STATE_UPDATED: u8 = 0b10; // 1
const STATE_USING: u8 =   0b01; // 2
*/
const STATE_READY: u8 = 0b000;     // 0 - this state indicates that an update can be performed
const STATE_IN_USE: u8 = 0b100;    // 1 - this state indicates that there is an update being performed
const STATE_UPDATABLE: u8 = 0b010; // 2 - this state indicates that there is a pending update
const STATE_UPDATING: u8 = 0b011;  // 6 - this state indicates that there is an external update being performed - this is intentionally the same value as
                                   // the `NORMALIZATION_MASK`
const NORMALIZATION_MASK: u8 = 0b011; // can be used to make all states equal except the in_use state

struct LocalData<T: Send + Sync, D: DataPtrConvert<T> = Arc<T>, const METADATA_PREFIX_BITS: u32 = 0> {
    parent: Arc<SwapArcIntermediateTLS<T, D, METADATA_PREFIX_BITS>>,
    state: AtomicU8,
    inner: UnsafeCell<LocalDataInner<T, D, METADATA_PREFIX_BITS>>,
}

struct LocalDataInner<T: Send + Sync, D: DataPtrConvert<T> = Arc<T>, const METADATA_PREFIX_BITS: u32 = 0> {
    new_val: ManuallyDrop</*Option<*/D/*>*/>,
    new_val_ptr: *const T,
    new_ref_cnt: usize,
    val: ManuallyDrop<D>,
    ref_cnt: usize,
}

/// SAFETY: Types implementing this trait are expected to perform
/// reference counting through cloning/dropping internally.
pub unsafe trait RefCnt: Send + Sync + Sync + Clone {}

pub trait DataPtrConvert<T>: RefCnt + Sized {

    const INVALID: *const T;

    /// This function may not alter the reference count of the
    /// reference counted "object".
    fn from(ptr: *const T) -> Self;

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
    fn increase_ref_cnt(&self);

}

unsafe impl<T: Send + Sync> RefCnt for Arc<T> {}

impl<T: Send + Sync> DataPtrConvert<T> for Arc<T> {
    const INVALID: *const T = null();

    fn from(ptr: *const T) -> Self {
        unsafe { Arc::from_raw(ptr) }
    }

    fn into(self) -> *const T {
        let ret = Arc::into_raw(self);
        // decrement the reference count
        <Self as DataPtrConvert<T>>::from(ret);
        ret
    }

    fn as_ptr(&self) -> *const T {
        Arc::as_ptr(self)
    }

    fn increase_ref_cnt(&self) {
        mem::forget(self.clone());
    }
}

unsafe impl<T: Send + Sync> RefCnt for Option<Arc<T>> {}

impl<T: Send + Sync> DataPtrConvert<T> for Option<Arc<T>> {
    const INVALID: *const T = null();

    fn from(ptr: *const T) -> Self {
        if !ptr.is_null() {
            Some(unsafe { Arc::from_raw(ptr) })
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
                <Self as DataPtrConvert<T>>::from(ret);
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

    fn increase_ref_cnt(&self) {
        mem::forget(self.clone());
    }
}
