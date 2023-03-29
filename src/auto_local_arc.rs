use crossbeam_utils::{Backoff, CachePadded};
use std::alloc::{alloc, dealloc, Layout, LayoutError};
use std::cell::UnsafeCell;
use std::mem::{align_of, size_of, ManuallyDrop, transmute};
use std::ops::Deref;
use std::process::abort;
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering, AtomicU8};
use std::{mem, ptr, thread};
use thread_local::ThreadLocal;

// FIXME: NOTE: the version of thread_local that is used changes the behavior of this!

// Debt(t) <= Debt(t + 1)
// Refs >= Debt
// Refs(t + 1) >= Debt(t)

pub struct AutoLocalArc<T: Send + Sync> {
    inner: *const InnerArc<T>,
    cache: UnsafeCell<*mut CachePadded<Cache<T>>>,
}

unsafe impl<T: Send + Sync> Send for AutoLocalArc<T> {}
unsafe impl<T: Send + Sync> Sync for AutoLocalArc<T> {}

impl<T: Send + Sync> AutoLocalArc<T> {
    pub fn new(val: T) -> Self {
        let inner = SizedBox::new(InnerArc {
            val,
            cache: Default::default(),
        })
        .into_ptr()
        .as_ptr();
        let cache = unsafe { &*inner }
            .cache
            .get_or(|| {
                // println!("inserting: {}", thread_id());
                DetachablePtr(
                    Some(SizedBox::new(CachePadded::new(Cache {
                        parent: inner,
                        ref_cnt: AtomicUsize::new(1),
                        debt: AtomicUsize::new(0),
                        thread_id: thread_id(),
                        src: AtomicPtr::new(null_mut()), // we have no src, as we are the src ourselves
                        finished: AtomicUsize::new(FinishedMsg::default().0),
                    }))
                    .into_ptr()),
                )
            })
            .0.unwrap().as_ptr();
        let ret = Self {
            inner,
            cache: cache.into(),
        };
        ret
    }

    #[inline(always)]
    fn inner(&self) -> &InnerArc<T> {
        unsafe { &*self.inner }
    }
}

/// We have to choose a lower highest ref cnt than the std lib as we are using the last 2 bits to store metadata
const MAX_SOFT_REFCOUNT: usize = MAX_HARD_REFCOUNT / 2;
const MAX_HARD_REFCOUNT: usize = isize::MAX as usize / 2_usize.pow(2);

impl<T: Send + Sync> Clone for AutoLocalArc<T> {
    #[inline]
    fn clone(&self) -> Self {
        let cache_ptr = unsafe { *self.cache.get() };
        let cache = unsafe { &*cache_ptr };
        // check if we are the owner of this alarc's cache

        let tid = thread_id();

        let cache = if cache.thread_id == tid {
            // we are the owner of this cache, just perform a simple increment

            let ref_cnt = cache.ref_cnt.load(Ordering::Relaxed/*Acquire*/);
            if ref_cnt > MAX_SOFT_REFCOUNT {
                unsafe {
                    handle_large_ref_count(cache, ref_cnt);
                }
            }
            cache.ref_cnt.store(ref_cnt + 1, Ordering::Release);
            cache_ptr
        } else {
            // we aren't the owner, now we have to do some more work

            let inner = self.inner;
            let local_cache_ptr = unsafe { self
                .inner()
                .cache
                .get_or(|| {
                    println!("inserting: {}", tid);
                    DetachablePtr(
                        Some(SizedBox::new(CachePadded::new(Cache {
                            parent: inner,
                            src: AtomicPtr::new(cache_ptr),
                            debt: AtomicUsize::new(0),
                            thread_id: tid,
                            ref_cnt: AtomicUsize::new(0),
                            finished: AtomicUsize::new(FinishedMsg::default().0),
                        }))
                        .into_ptr()),
                    )
                })
                .0.unwrap_unchecked().as_ptr() };
            let local_cache = unsafe { &*local_cache_ptr };
            // the ref count here can't change, as we are the only thread which is able to access it.
            let ref_cnt = local_cache.ref_cnt.load(Ordering::Relaxed/*Acquire*/);
            let debt = local_cache.debt.load(Ordering::Acquire);

            // check if the local cache's instance is still valid.
            if ref_cnt == debt {
                if debt > 0 {
                    // we have a retry loop here in case the debt's updater hasn't finished yet
                    let mut finish_cnt = local_cache.wait_for::<false>(ref_cnt);
                    if finish_cnt.ty() != FinishedMsgTy::Deleted && finish_cnt.ty() != FinishedMsgTy::Finished {
                        println!("battle race!");
                        // the other thread didn't see our increment in time, so we have to wait!
                        cleanup_cache::<true, T>(local_cache_ptr, false, finish_cnt.id());
                    }
                }
                // the local cache has no valid references anymore
                local_cache.src.store(cache_ptr, Ordering::Release);
                                                                     // this is `2` because we need a reference for the current thread's instance and
                                                                     // because the newly created instance needs a reference as well.
                local_cache.ref_cnt.store(2, Ordering::Release);
                // we update the debt after the ref_cnt because that enables us to see the `ref_cnt` update by loading `debt` using `Acquire`
                local_cache.debt.store(0, Ordering::Release);
                local_cache.finished.store(FinishedMsg::default().0, Ordering::Release);
                // remove the reference to the external cache, as we moved it to our own cache instead
                *unsafe { &mut *self.cache.get() } = local_cache_ptr;
            } else {
                // the local cache is still in use, try simply incrementing its counter
                // if we fail, just fall back to the slow path
                if ref_cnt > MAX_SOFT_REFCOUNT {
                    unsafe {
                        handle_large_ref_count(local_cache, ref_cnt);
                    }
                }
                local_cache.ref_cnt.store(ref_cnt + 1, Ordering::Release);
                // sync with the local store
                fence(Ordering::Acquire); // FIXME: is this even required?
                // FIXME: here is probably a possibility for a race condition (what if we have one of the instances on another thread and it gets freed right
                // FIXME: after we check if ref_cnt == debt?) this can probably be fixed by checking again right after increasing the ref_count and
                // FIXME: handling failure by doing what we would have done if the local cache had no valid references to begin with.
                let debt = local_cache.debt.load(Ordering::Acquire);
                // we don't need to strip the flags here as we know that `debt` isn't detached.
                if debt == ref_cnt {
                    if debt > 0 {
                        // we have a retry loop here in case the debt's updater hasn't finished yet
                        let finish_cnt = local_cache.wait_for::<false>(ref_cnt);
                        if finish_cnt.ty() != FinishedMsgTy::Deleted {
                            println!("battle race!");
                            // the other thread didn't see our increment in time, so we have to wait!
                            cleanup_cache::<true, T>(local_cache_ptr, false, finish_cnt.id());
                        }
                    }
                    println!("racing load!");
                    // the local cache has no valid references anymore
                    local_cache.src.store(cache_ptr, Ordering::Release);
                                                                         // this is `2` because we need a reference for the current thread's instance and
                                                                         // because the newly created instance needs a reference as well.
                    local_cache.ref_cnt.store(2, Ordering::Release);
                    // we update the debt after the ref_cnt because that enables us to see the `ref_cnt` update by loading `debt` using `Acquire`
                    local_cache.debt.store(0, Ordering::Release);
                    local_cache.finished.store(FinishedMsg::default().0, Ordering::Release);
                    // remove the reference to the external cache, as we moved it to our own cache instead
                    *unsafe { &mut *self.cache.get() } = local_cache_ptr;
                }
            }

            local_cache_ptr
        };

        Self {
            inner: self.inner,
            cache: cache.into(),
        }
    }
}

/// Safety: this method may only be called with the local cache as the `cache` parameter.
#[cold]
unsafe fn handle_large_ref_count<T: Send + Sync>(cache: &CachePadded<Cache<T>>, ref_cnt: usize) {
    panic!("error!");
    // try to recover by decrementing the `debt` count from the `ref_cnt` and setting the `debt` count to 0
    // note: the `detached` flag can't be set because this method is only called with the `local_cache` as the `cache` parameter
    let debt = cache.debt.swap(0, Ordering::Relaxed); // FIXME: can the lack of synchronization between these two updates lead to race conditions?
    let ref_cnt = ref_cnt - debt;
    cache.ref_cnt.store(ref_cnt, Ordering::Relaxed);
    if ref_cnt > MAX_HARD_REFCOUNT {
        abort();
    }
}

impl<T: Send + Sync> Drop for AutoLocalArc<T> {
    #[inline]
    fn drop(&mut self) {
        let cache_ptr = unsafe { *self.cache.get() };
        let cache = unsafe { &*cache_ptr };
        let tid = thread_id();
        if cache.thread_id == tid {
            if cache.thread_id == 0 {
               println!("start dropping local!");
                for _ in 0..10 {
                    println!("eeee");
                }
            }
            let ref_cnt = cache.ref_cnt.load(Ordering::Relaxed);
            // println!("dropping {}", ref_cnt);
            cache.ref_cnt.store(ref_cnt - 1, Ordering::Release);
            let ref_cnt = ref_cnt - 1;
            // synchronize with ref_cnt in order for future loads to occur after the local store.
            fence(Ordering::Acquire);
            // `ref_cnt` can't race with `debt` here because we are the only ones who are able to update `ref_cnt`
            let debt = cache.debt.load(Ordering::Acquire);
            // let fin = cache.finish_cnt.load(Ordering::Acquire);
            if cache.thread_id == 0 {
                println!(
                    "local drop: {} refs: {} debt: {}",
                    cache.thread_id, ref_cnt, debt/*, fin*/
                );
            }
            // println!("debt: {}\nref_cnt: {}", strip_flags(debt), ref_cnt);
            // TODO: we could add a fastpath here for debt == 0
            if ref_cnt == strip_flags(debt) {
                // we have a retry loop here in case the debt's updater hasn't finished yet
                let finish_cnt = cache.wait_for::<false>(ref_cnt);
                if finish_cnt.ty() != FinishedMsgTy::Deleted && finish_cnt.ty() != FinishedMsgTy::Finished {
                    // we know that the thread we waited on freed the reference
                    drop(cache);
                    // there are no external refs alive
                    cleanup_cache::<true, T>(cache_ptr, is_detached(debt), finish_cnt.id());
                } else {
                    // FIXME: do we have to do smth here?
                    println!("weird other thingy \"smth\"!");
                }
            }
        } else {
            println!("non-local cache ptr drop!");
            let debt = cache.debt.fetch_add(1, Ordering::AcqRel) + 1;
            if strip_flags(debt) > MAX_HARD_REFCOUNT {
                abort();
            }
            fence(Ordering::Acquire);
            let ref_cnt = cache.ref_cnt.load(Ordering::Acquire);
            if strip_flags(debt) == ref_cnt {
                // FIXME: ref_cnt can probably race with `debt` here
                // FIXME: so what we need to do is add further validation after this check
                // FIXME: this is probably in form of HazardPointers or the like
                // we have a retry loop here in case the debt's updater hasn't finished yet
                let finish_cnt = cache.wait_for::<true>(ref_cnt);

                // FIXME: NEW: there is probably a race condition here - what if cache's owner increases its reference count here
                // FIXME: and we free its cache just down below?

                drop(cache);
                // there are no external refs alive
                if !cleanup_cache::<true, T>(cache_ptr, is_detached(debt), finish_cnt.id()) {
                    cache.finished.store(FinishedMsg::new(ref_cnt, FinishedMsgTy::Finished).0, Ordering::Release);
                } else {
                    cache.finished.store(FinishedMsg::new(ref_cnt, FinishedMsgTy::Deleted).0, Ordering::Release);
                }
            }
        }
    }
}

impl<T: Send + Sync> AsRef<T> for AutoLocalArc<T> {
    #[inline(always)]
    fn as_ref(&self) -> &T {
        unsafe { &*self.inner.cast::<T>() }
    }
}

impl<T: Send + Sync> Deref for AutoLocalArc<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inner.cast::<T>() }
    }
}

#[repr(C)]
struct InnerArc<T: Send + Sync> {
    val: T,
    cache: ThreadLocal<DetachablePtr<T>>,
}

impl<T: Send + Sync> Drop for InnerArc<T> {
    #[inline]
    fn drop(&mut self) {
        println!("dropping central struct!");
        // wait for all caches to finish.
        for cache in self.cache.iter() {
            let mut backoff = Backoff::new();
            loop {
                let msg = FinishedMsg(unsafe { &*cache.0.unwrap().as_ptr() }.finished.load(Ordering::Acquire));
                if msg.ty() == FinishedMsgTy::Finished || msg.ty() == FinishedMsgTy::Deleted { // FIXME: do we need to accept `Deleted` as well?
                    break;
                }
                backoff.snooze();
            }
        }

        for cache in self.cache.iter_mut() {
            unsafe { cache.cleanup(); }
        }
    }
}

#[derive(Copy, Clone, PartialEq, Default)]
#[repr(usize)]
enum FinishedMsgTy {
    #[default]
    NotFinished = 0,
    Finished = 1,
    NotDeleted = 2,
    Deleted = 3, // this means the cache got deleted
}

impl FinishedMsgTy {

    #[inline]
    fn into_raw(self) -> usize {
        self as usize
    }

    #[inline]
    unsafe fn from_raw(raw: usize) -> Self {
        transmute(raw)
    }

}

const FINISHED_MSG_TY_MASK: usize = (1 << (usize::BITS - 1)) | (1 << (usize::BITS - 2));
const FINISHED_MSG_TY_OFFSET: u32 = usize::BITS - 2;

#[derive(Copy, Clone, Default)]
struct FinishedMsg(usize);

impl FinishedMsg {

    #[inline]
    fn new(id: usize, ty: FinishedMsgTy) -> Self {
        Self(id | (ty.into_raw() << FINISHED_MSG_TY_OFFSET))
    }

    #[inline]
    fn ty(self) -> FinishedMsgTy {
        unsafe { FinishedMsgTy::from_raw((self.0 & FINISHED_MSG_TY_MASK) >> FINISHED_MSG_TY_OFFSET) }
    }

    #[inline]
    fn id(self) -> usize {
        self.0 & !FINISHED_MSG_TY_MASK
    }

}

#[repr(C)]
struct Cache<T: Send + Sync> {
    parent: *const InnerArc<T>,
    thread_id: u64,
    src: AtomicPtr<CachePadded<Cache<T>>>,
    debt: AtomicUsize, // this debt count may only be updated by threads other than the current one - this has a `detached` flag as its last bit
    // FIXME: move this to the metadata!
    ref_cnt: AtomicUsize, // this ref cnt may only be updated by the current thread
    finished: AtomicUsize, // this contains the finish_cnt of the messenger and the message type in the last two bits (see `Finished` for their meaning)
}

impl<T: Send + Sync> Cache<T> {
    fn wait_for<const OWNS_LOCAL_REF: bool>(&self, ref_cnt: usize) -> FinishedMsg {
        let mut finish_msg = FinishedMsg(self.finished.load(Ordering::Acquire));
        let local_add = if OWNS_LOCAL_REF { 1 } else { 0 };
        while ref_cnt != finish_msg.id() + local_add {
            finish_msg = FinishedMsg(self.finished.load(Ordering::Acquire));
        }
        finish_msg
    }
}

fn cleanup_cache<const UPDATE_SUPER: bool, T: Send + Sync>(
    cache: *const CachePadded<Cache<T>>,
    detached: bool,
    msg_id: usize,
) -> bool {
    println!("cleanup cache {}", detached);
    if UPDATE_SUPER {
        // FIXME: when freeing the own memory, we still have a reference to it and thus UB
        // FIXME: the same thing is the case when freeing the major allocation
        // println!("cleanup cache: {}", unsafe { &*cache }.ref_cnt.load(Ordering::Relaxed));
        // FIXME: check for `detached` and deallocate own memory if detached


        // ORDERING: this is Acquire because we have to link to the previous store to `src` which might have
        // happened on a different thread than this (the deallocating) one.
        let src = unsafe { &*cache }.src.load(Ordering::Acquire);
        // there are no external refs alive
        if src.is_null() {
            println!("delete main thingy: {}", thread_id());
            unsafe { &*cache }
                .debt
                .fetch_or(DETACHED_FLAG, Ordering::AcqRel);
            unsafe { &*cache }.finished.store(FinishedMsg::new(msg_id, FinishedMsgTy::Finished).0, Ordering::Release);
            fence(Ordering::Acquire);

            // we are the first "(cache) node", so we need to free the memory
            unsafe {
                drop_slow::<T>(unsafe { &*cache }.parent.cast_mut());
            }

            #[cold]
            unsafe fn drop_slow<T: Send + Sync>(ptr: *mut InnerArc<T>) {
                drop(unsafe { SizedBox::from_ptr(NonNull::new_unchecked(ptr)) });
            }
            return true;
        } else {
            // FIXME: we have other caches above us, so release a reference to them - this has to happen recursively tho as they could have to release their last reference as well.
            // FIXME: this should work - now make it iterative instead of recursive!

            let super_cache_ptr = src;
            let super_cache = unsafe { &*super_cache_ptr };
            let debt = super_cache.debt.fetch_add(1, Ordering::AcqRel) + 1; // we add 1 at the end to make sure we use the post-update value
            /*// this fence protects the following load of `ref_cnt` from being moved before the guard increment.
            fence(Ordering::Acquire);*/

            // when `DetachablePtr` gets dropped, it will check if it has to free the allocation itself and thus
            // if we observe that `DetachablePtr` was dropped and because we are still alive it didn't free the allocation,
            // we know that we have to free the allocation ourselves.
            let super_detached = is_detached(debt);
            // because of this load after incrementing the debt, we have to have another fetch_add
            // operation later on to signal to other threads that our load finished.
            // FIXME: as the thread_local crate doesn't deallocate its entries anyway we could try storing metadata in
            // FIXME: the entries in order to get rid of this last fetch_add
            let ref_cnt = super_cache.ref_cnt.load(Ordering::Acquire);

            // check if we should cleanup the super cache
            if strip_flags(debt) == ref_cnt {
                // we have a retry loop here in case the debt's updater hasn't finished yet
                let finish_cnt = super_cache.wait_for::<true>(ref_cnt); // FIXME: can a data race occur here?
                // fence(Ordering::AcqRel);

                drop(super_cache);

                fence(Ordering::AcqRel);
                if !cleanup_cache::<true, T>(super_cache_ptr, super_detached, finish_cnt.id()) {
                    // we only update the finish_cnt if the cleanup performed isn't final
                    // i.e if the backing allocation wasn't freed.

                    // fence(Ordering::AcqRel);
                    super_cache
                        .finished
                        .store(FinishedMsg::new(debt, FinishedMsgTy::Finished).0, Ordering::Release);
                } else {
                    super_cache.finished.store(FinishedMsg::new(debt, FinishedMsgTy::Deleted).0, Ordering::Release);
                }
                // fence(Ordering::AcqRel);
            }
            // unsafe { cache.as_ref().unwrap() }.finished.store(FinishedMsg::new(debt, FinishedMsgTy::Finished).0, Ordering::Release); // FIXME: do we have to do this?
        }
    }
    if detached {
        // ORDERING: We use a fence here in order to ensure that all other operations and uses
        // of atomics happen-before this.
        fence(Ordering::Acquire);
        unsafe { SizedBox::from_ptr(NonNull::new_unchecked(cache.cast_mut())) };
    }
    detached
}

struct Metadata {
    thread_id: AtomicUsize,
    ref_cnt: AtomicUsize,
}

unsafe impl<T: Send + Sync> Send for Cache<T> {}
unsafe impl<T: Send + Sync> Sync for Cache<T> {}

#[derive(Clone)]
#[repr(transparent)]
struct DetachablePtr<T: Send + Sync>(Option<NonNull<CachePadded<Cache<T>>>>);

unsafe impl<T: Send + Sync> Send for DetachablePtr<T> {}
unsafe impl<T: Send + Sync> Sync for DetachablePtr<T> {}

impl<T: Send + Sync> DetachablePtr<T> {

    unsafe fn cleanup(&mut self) {
        if let Some(cache) = self.0.take() {
            cleanup_cache::<false, T>(cache.as_ptr(), true, 0);
        }
    }

}

impl<T: Send + Sync> Drop for DetachablePtr<T> {
    #[inline]
    fn drop(&mut self) {
        // println!("dropping detachable ptr: {}", cache.thread_id);
        // if !is_detached(cache.debt.load(Ordering::Acquire)) {
        if let Some(cache_ptr) = self.0 {
            let cache = unsafe { &*cache_ptr.as_ptr() };
            // println!("dropping detachable ptr!");
            // unsafe { &*self.0 }.detached.store(true, Ordering::Relaxed);

            // ORDERING: This is `Acquire` as the dropping thread
            // doesn't necessarily have to be the same as the one
            // owning the cache and we have to make sure that all decrements happened before this.
            let ref_cnt = cache.ref_cnt.load(Ordering::Acquire);
            // mark the cache as detached in order to allow other threads to see
            let debt = cache.debt.fetch_or(DETACHED_FLAG, Ordering::AcqRel); // TODO: try avoiding having to do this RMW for most cases by adding a fast path
            // FIXME: are we sure that ref_cnt's last desired value is already visible to us here?
            if debt == ref_cnt {
                // fence(Ordering::AcqRel);
                // we have a retry loop here in case the debt's updater hasn't finished yet
                cache.wait_for::<false>(ref_cnt);
                let cache_ptr = cache_ptr.as_ptr();
                drop(cache);
                fence(Ordering::AcqRel);
                cleanup_cache::<false, T>(cache_ptr, true, 0); // FIXME: do we have to set a marker here?
            }
        }
    }
}

#[inline]
fn thread_id() -> u64 {
    // thread::current().id().as_u64().get()
    thread_id::get() as u64
}

// this is for `debt` and indicates that the thread local cache containing
// the DetachablePtr is being dropped or got dropped.
const DETACHED_FLAG: usize = 1 << (usize::BITS - 1);

#[inline(always)]
const fn strip_flags(debt: usize) -> usize {
    debt & !DETACHED_FLAG
}

#[inline(always)]
const fn is_detached(debt: usize) -> bool {
    debt & DETACHED_FLAG != 0
}

struct SizedBox<T> {
    alloc_ptr: NonNull<T>,
}

impl<T> SizedBox<T> {
    const LAYOUT: Layout = {
        Layout::new::<T>()
    };

    fn new(val: T) -> Self {
        // SAFETY: The layout we provided was checked at compiletime, so it has to be initialized correctly
        let alloc = unsafe { alloc(Self::LAYOUT) }.cast::<T>();
        // FIXME: add safety comment
        unsafe {
            alloc.write(val);
        }
        Self {
            alloc_ptr: NonNull::new(alloc).unwrap(), // FIXME: can we make this unchecked?
        }
    }

    #[inline]
    fn as_ref(&self) -> &T {
        // SAFETY: This is safe because we know that alloc_ptr can't be zero
        // and because we know that alloc_ptr has to point to a valid
        // instance of T in memory
        unsafe { &*self.alloc_ptr.as_ptr() }
    }

    #[inline]
    fn as_mut(&mut self) -> &mut T {
        // SAFETY: This is safe because we know that alloc_ptr can't be zero
        // and because we know that alloc_ptr has to point to a valid
        // instance of T in memory
        unsafe { &mut *self.alloc_ptr.as_ptr() }
    }

    #[inline]
    fn into_ptr(self) -> NonNull<T> {
        let ret = self.alloc_ptr;
        mem::forget(self);
        ret
    }

    #[inline]
    unsafe fn from_ptr(ptr: NonNull<T>) -> Self {
        Self { alloc_ptr: ptr }
    }
}

impl<T> Drop for SizedBox<T> {
    fn drop(&mut self) {
        // SAFETY: This is safe to call because SizedBox can only be dropped once
        unsafe {
            ptr::drop_in_place(self.alloc_ptr.as_ptr());
        }
        // FIXME: add safety comment
        unsafe {
            dealloc(self.alloc_ptr.as_ptr().cast::<u8>(), SizedBox::<T>::LAYOUT);
        }
    }
}
