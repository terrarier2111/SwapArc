use crossbeam_utils::{Backoff, CachePadded};
use std::alloc::{alloc, dealloc, Layout, LayoutError};
use std::cell::UnsafeCell;
use std::mem::{align_of, size_of, ManuallyDrop, transmute};
use std::ops::Deref;
use std::process::abort;
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering, AtomicU8, AtomicU64};
use std::{mem, ptr, thread};
use lazy_static::lazy_static;
use likely_stable::unlikely;
use thread_local::ThreadLocal;

// FIXME: NOTE: the version of thread_local that is used changes the behavior of this!

// Debt(t) <= Debt(t + 1)
// Refs >= Debt
// Refs(t + 1) >= Debt(t)

pub struct AutoLocalArc<T: Send + Sync> {
    inner: NonNull<InnerArc<T>>,
    cache: UnsafeCell<NonNull<CachePadded<Cache<T>>>>,
}

unsafe impl<T: Send + Sync> Send for AutoLocalArc<T> {}
unsafe impl<T: Send + Sync> Sync for AutoLocalArc<T> {}

impl<T: Send + Sync> AutoLocalArc<T> {
    pub fn new(val: T) -> Self {
        let inner = SizedBox::new(InnerArc {
            val,
            cache: Default::default(),
        })
        .into_ptr();
        let cache = unsafe { inner.as_ref() }
            .cache
            .get_val_and_meta_or(|meta| {
                // println!("inserting: {}", thread_id());
                DetachablePtr(
                    Some(SizedBox::new(CachePadded::new(Cache {
                        parent: inner,
                        thread_id: thread_id(),
                        src: AtomicPtr::new(null_mut()), // we have no src, as we are the src ourselves
                        guard: unsafe { NonNull::new_unchecked((GUARDS.get_or_default() as *const AtomicPtr<()>).cast_mut()) },
                        meta,
                    }))
                        .into_ptr()),
                )
            }, |meta| {
                meta.ref_cnt.store(1, Ordering::Release);
                meta.thread_id.store(thread_id(), Ordering::Release);
            }).0.0.unwrap();
        let ret = Self {
            inner,
            cache: cache.into(),
        };
        ret
    }

    #[inline(always)]
    fn inner(&self) -> &InnerArc<T> {
        unsafe { self.inner.as_ref() }
    }
}

/// We have to choose a lower highest ref cnt than the std lib as we are using the last 2 bits to store metadata
const MAX_SOFT_REFCOUNT: usize = MAX_HARD_REFCOUNT / 2;
const MAX_HARD_REFCOUNT: usize = isize::MAX as usize / 2_usize.pow(2);

impl<T: Send + Sync> Clone for AutoLocalArc<T> {
    #[inline]
    fn clone(&self) -> Self {
        let cache_ptr = unsafe { *self.cache.get() };
        let cache = unsafe { cache_ptr.as_ref() };
        let meta_ptr = cache.meta;
        let meta = unsafe { &*meta_ptr };

        // check if we are the owner of this alarc's cache

        let tid = thread_id();

        let cache = if cache.thread_id == tid {
            // we are the owner of this cache, just perform a simple increment

            let ref_cnt = meta.ref_cnt.load(Ordering::Relaxed/*Acquire*/);
            if ref_cnt > MAX_SOFT_REFCOUNT {
                unsafe {
                    handle_large_ref_count((cache as *const CachePadded<Cache<T>>).cast_mut(), ref_cnt);
                }
            }
            meta.ref_cnt.store(ref_cnt + 1, Ordering::Release);
            cache_ptr
        } else {
            // we aren't the owner, now we have to do some more work

            let inner = self.inner;
            let (local_cache_ptr, local_meta) = unsafe { self
                .inner()
                .cache
                .get_val_and_meta_or(|meta| {
                    println!("inserting: {}", tid);
                    DetachablePtr(
                        Some(SizedBox::new(CachePadded::new(Cache {
                            parent: inner,
                            src: AtomicPtr::new(cache_ptr.as_ptr()),
                            thread_id: tid,
                            guard: unsafe { NonNull::new_unchecked((GUARDS.get_or_default() as *const AtomicPtr<()>).cast_mut()) },
                            meta,
                        }))
                        .into_ptr()),
                    )
                }, |meta| {/*
                    meta.thread_id.store(tid, Ordering::Release);
                */}) };
            let local_cache_ptr = unsafe { local_cache_ptr.0.unwrap_unchecked() };
            let local_cache = unsafe { local_cache_ptr.as_ref() };
            // the ref count here can't change, as we are the only thread which is able to access it.
            let ref_cnt = local_meta.ref_cnt.load(Ordering::Relaxed/*Acquire*/);
            let debt = local_meta.debt.load(Ordering::Acquire);

            // check if the local cache's instance is still valid.
            if ref_cnt == debt {
                if debt > 0 {
                    // we have a retry loop here in case the debt's updater hasn't finished yet
                    let mut backoff = Backoff::new();
                    // wait for the other thread to clean up
                    while local_meta.thread_id.load(Ordering::Acquire) != INVALID_TID {
                        backoff.snooze();
                    }
                }
                // the local cache has no valid references anymore
                local_cache.src.store(cache_ptr.as_ptr(), Ordering::Release);
                                                                     // this is `2` because we need a reference for the current thread's instance and
                                                                     // because the newly created instance needs a reference as well.
                meta.ref_cnt.store(2, Ordering::Release);
                // we update the debt after the ref_cnt because that enables us to see the `ref_cnt` update by loading `debt` using `Acquire`
                meta.debt.store(0, Ordering::Release);
                meta.thread_id.store(tid, Ordering::Release);
                // remove the reference to the external cache, as we moved it to our own cache instead
                *unsafe { &mut *self.cache.get() } = local_cache_ptr;
            } else {
                // the local cache is still in use, try simply incrementing its counter
                // if we fail, just fall back to the slow path
                if ref_cnt > MAX_SOFT_REFCOUNT {
                    unsafe {
                        handle_large_ref_count((local_cache as *const CachePadded<Cache<T>>).cast_mut(), ref_cnt);
                    }
                }
                meta.ref_cnt.store(ref_cnt + 1, Ordering::Release);
                // sync with the local store
                fence(Ordering::Acquire); // FIXME: is this even required?
                // FIXME: here is probably a possibility for a race condition (what if we have one of the instances on another thread and it gets freed right
                // FIXME: after we check if ref_cnt == debt?) this can probably be fixed by checking again right after increasing the ref_count and
                // FIXME: handling failure by doing what we would have done if the local cache had no valid references to begin with.
                let debt = meta.debt.load(Ordering::Acquire);
                // we don't need to strip the flags here as we know that `debt` isn't detached.
                if debt == ref_cnt {
                    if debt > 0 {
                        // we have a retry loop here in case the debt's updater hasn't finished yet
                        let mut backoff = Backoff::new();
                        // wait for the other thread to clean up
                        while local_meta.thread_id.load(Ordering::Acquire) != INVALID_TID {
                            backoff.snooze();
                        }
                    }
                    println!("racing load!");
                    // the local cache has no valid references anymore
                    local_cache.src.store(cache_ptr.as_ptr(), Ordering::Release);
                                                                         // this is `2` because we need a reference for the current thread's instance and
                                                                         // because the newly created instance needs a reference as well.
                    meta.ref_cnt.store(2, Ordering::Release);
                    // we update the debt after the ref_cnt because that enables us to see the `ref_cnt` update by loading `debt` using `Acquire`
                    meta.debt.store(0, Ordering::Release);
                    meta.thread_id.store(tid, Ordering::Release);
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
unsafe fn handle_large_ref_count<T: Send + Sync>(cache: *mut CachePadded<Cache<T>>, ref_cnt: usize) {
    panic!("error!");

    let meta = unsafe { cache.as_ref().unwrap_unchecked().meta.as_ref().unwrap_unchecked() };

    // try to recover by decrementing the `debt` count from the `ref_cnt` and setting the `debt` count to 0
    // note: the `detached` flag can't be set because this method is only called with the `local_cache` as the `cache` parameter
    let debt = meta.debt.swap(0, Ordering::Relaxed); // FIXME: can the lack of synchronization between these two updates lead to race conditions?
    let ref_cnt = ref_cnt - debt;
    meta.ref_cnt.store(ref_cnt, Ordering::Relaxed);
    if ref_cnt > MAX_HARD_REFCOUNT {
        abort();
    }
}

impl<T: Send + Sync> Drop for AutoLocalArc<T> {
    #[inline]
    fn drop(
        &mut self) {
        let cache_ptr = unsafe { *self.cache.get() };
        let cache = unsafe { cache_ptr.as_ref() };
        let meta_ptr = unsafe { cache_ptr.as_ref().meta };
        let meta = unsafe { &*meta_ptr };
        let tid = thread_id();
        if cache.thread_id == tid {
            if cache.thread_id == 0 {
               println!("start dropping local!");
                for _ in 0..10 {
                    println!("eeee");
                }
            }
            let ref_cnt = meta.ref_cnt.load(Ordering::Relaxed);
            // println!("dropping {}", ref_cnt);
            let guard = unsafe { cache.guard.as_ref() };
            guard.store(cache_ptr.as_ptr().cast(), Ordering::Release);
            meta.ref_cnt.store(ref_cnt - 1, Ordering::Release);
            let ref_cnt = ref_cnt - 1;
            // synchronize with ref_cnt in order for future loads to occur after the local store.
            fence(Ordering::Acquire);
            // `ref_cnt` can't race with `debt` here because we are the only ones who are able to update `ref_cnt`
            let debt = meta.debt.load(Ordering::Acquire);
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
                cleanup_cache::<true, T>(cache_ptr, is_detached(debt), tid); // FIXME: we don't need to check for an expected TID here.
            }
            guard.store(null_mut(), Ordering::Release);
        } else {
            println!("non-local cache ptr drop!");
            let guard = GUARDS.get_or_default();
            guard.store(cache_ptr.as_ptr().cast(), Ordering::Release);
            let cache_tid = cache.thread_id;
            let debt = meta.debt.fetch_add(1, Ordering::AcqRel) + 1;
            if strip_flags(debt) > MAX_HARD_REFCOUNT {
                abort();
            }
            fence(Ordering::Acquire);
            let ref_cnt = unsafe { cache.meta.as_ref().unwrap_unchecked() }.ref_cnt.load(Ordering::Acquire);
            if strip_flags(debt) == ref_cnt {

                // FIXME: NEW: there is probably a race condition here - what if cache's owner increases its reference count here
                // FIXME: and we free its cache just down below?

                drop(cache);
                // there are no external refs alive
                cleanup_cache::<true, T>(cache_ptr, is_detached(debt), cache_tid);
            }
            guard.store(null_mut(), Ordering::Release);
        }
    }
}

impl<T: Send + Sync> Deref for AutoLocalArc<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        let offset = memoffset::offset_of!(InnerArc<T>, val);
        unsafe { &*self.inner.cast::<T>().as_ptr().byte_add(offset) }
    }
}

#[repr(C)]
struct InnerArc<T: Send + Sync> {
    val: T,
    cache: ThreadLocal<DetachablePtr<T>, Metadata>,
}

impl<T: Send + Sync> Drop for InnerArc<T> {
    #[inline]
    fn drop(&mut self) {
        println!("dropping central struct!");
        // wait for all caches to finish.
        for cache in GUARDS.iter() {
            let mut backoff = Backoff::new();
            while cache.load(Ordering::Acquire).cast_const() == (self as *const InnerArc<T>).cast() {
                backoff.snooze();
            }
        }

        for cache in self.cache.iter_mut() {
            unsafe { cache.cleanup(); }
        }
    }
}

#[repr(C)]
struct Cache<T: Send + Sync> {
    parent: NonNull<InnerArc<T>>,
    guard: NonNull<AtomicPtr<()>>,
    thread_id: u64,
    meta: *const Metadata, // FIXME: make this NonNull
    src: AtomicPtr<CachePadded<Cache<T>>>,
}

fn cleanup_cache<const UPDATE_SUPER: bool, T: Send + Sync>(
    cache: NonNull<CachePadded<Cache<T>>>,
    detached: bool,
    exp_tid: u64,
) -> bool {
    let meta_ptr = unsafe { cache.as_ref().meta };
    let meta = unsafe { &*meta_ptr };

    if unlikely(meta.thread_id.compare_exchange(exp_tid, INVALID_TID, Ordering::AcqRel, Ordering::Relaxed).is_err()) {
        println!("wrong tid!");
        return false;
    }

    println!("cleanup cache {}", detached);
    if UPDATE_SUPER {
        // FIXME: when freeing the own memory, we still have a reference to it and thus UB
        // FIXME: the same thing is the case when freeing the major allocation
        // println!("cleanup cache: {}", unsafe { &*cache }.ref_cnt.load(Ordering::Relaxed));
        // FIXME: check for `detached` and deallocate own memory if detached


        // ORDERING: this is Acquire because we have to link to the previous store to `src` which might have
        // happened on a different thread than this (the deallocating) one.
        let src = unsafe { cache.as_ref() }.src.load(Ordering::Acquire);
        // there are no external refs alive
        if src.is_null() {
            println!("delete main thingy: {}", thread_id());
            meta
                .debt
                .fetch_or(DETACHED_FLAG, Ordering::AcqRel);
            // unsafe { cache.as_ref() }.finished.store(FinishedMsg::new(msg_id, FinishedMsgTy::Finished).0, Ordering::Release);
            fence(Ordering::Acquire);

            // we are the first "(cache) node", so we need to free the memory
            unsafe {
                drop_slow::<T>(unsafe { cache.as_ref() }.parent);
            }

            #[cold]
            unsafe fn drop_slow<T: Send + Sync>(ptr: NonNull<InnerArc<T>>) {
                drop(unsafe { SizedBox::from_ptr(ptr) });
            }
            return true;
        } else {
            // FIXME: we have other caches above us, so release a reference to them - this has to happen recursively tho as they could have to release their last reference as well.
            // FIXME: this should work - now make it iterative instead of recursive!

            let super_cache_ptr = unsafe { NonNull::new_unchecked(src) };
            let super_cache = unsafe { super_cache_ptr.as_ref() };
            let super_meta_ptr = unsafe { super_cache.meta };
            let super_meta = unsafe { &*super_meta_ptr };
            let super_tid = super_cache.thread_id;
            let debt = super_meta.debt.fetch_add(1, Ordering::AcqRel) + 1; // we add 1 at the end to make sure we use the post-update value
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
            let ref_cnt = super_meta.ref_cnt.load(Ordering::Acquire);

            // check if we should cleanup the super cache by first checking the reference count and then checking if
            // the super cache cleaned up by itself
            if strip_flags(debt) == ref_cnt/* && super_thread_id == super_meta.thread_id.load(Ordering::Acquire)*/ {
                drop(super_cache);

                fence(Ordering::AcqRel);
                cleanup_cache::<true, T>(super_cache_ptr, super_detached, super_tid);
                // fence(Ordering::AcqRel);
            }
            // unsafe { cache.as_ref().unwrap() }.finished.store(FinishedMsg::new(debt, FinishedMsgTy::Finished).0, Ordering::Release); // FIXME: do we have to do this?
        }
    }
    if detached {
        // ORDERING: We use a fence here in order to ensure that all other operations and uses
        // of atomics happen-before this.
        fence(Ordering::Acquire);
        unsafe { SizedBox::from_ptr(cache) };
    }
    detached
}

#[derive(Default)]
struct Metadata {
    thread_id: AtomicU64,
    ref_cnt: AtomicUsize, // this ref cnt may only be updated by the current thread
    debt: AtomicUsize, // this debt count may only be updated by threads other than the current one - this has a `detached` flag as its last bit
}

impl thread_local::Metadata for Metadata {
    fn set_default(&self) {
        self.thread_id.store(thread_id(), Ordering::Release);
        self.ref_cnt.store(1, Ordering::Release);
    }
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
        println!("trying to clean up!");
        if let Some(cache) = self.0.take() {
            println!("cleaning up....");
            cleanup_cache::<false, T>(cache, true, cache.as_ref().thread_id);
        }
    }

}

impl<T: Send + Sync> Drop for DetachablePtr<T> {
    #[inline]
    fn drop(&mut self) {
        // println!("dropping detachable ptr: {}", cache.thread_id);
        // if !is_detached(cache.debt.load(Ordering::Acquire)) {
        if let Some(cache_ptr) = self.0 {
            let cache = unsafe { cache_ptr.as_ref() };
            let meta_ptr = unsafe { TLocal::<T>::val_meta_ptr_from_val(NonNull::new_unchecked(self as *mut Self)) };
            let meta = unsafe { meta_ptr.meta_ptr().as_ref() };
            // println!("dropping detachable ptr!");
            // unsafe { &*self.0 }.detached.store(true, Ordering::Relaxed);

            // ORDERING: This is `Acquire` as the dropping thread
            // doesn't necessarily have to be the same as the one
            // owning the cache and we have to make sure that all decrements happened before this.
            let ref_cnt = meta.ref_cnt.load(Ordering::Acquire);
            let tid = cache.thread_id;
            // mark the cache as detached in order to allow other threads to see
            let debt = meta.debt.fetch_or(DETACHED_FLAG, Ordering::AcqRel); // TODO: try avoiding having to do this RMW for most cases by adding a fast path
            // FIXME: are we sure that ref_cnt's last desired value is already visible to us here?
            if debt == ref_cnt {
                // we don't have to wait for other threads to finish as they will access
                // the cache which is available as long as there are threads accessing the
                // central data structure and not the backing allocation which can get deallocated
                // at any point in time.
                drop(cache);
                fence(Ordering::AcqRel);
                cleanup_cache::<false, T>(cache_ptr, true, tid); // FIXME: do we have to set a marker here?
            }
        }
    }
}

type TLocal<T> = ThreadLocal<DetachablePtr<T>, Metadata>;

const INVALID_TID: u64 = u64::MAX;

#[inline]
fn thread_id() -> u64 {
    // let tid = thread::current().id().as_u64().get();
    let tid = thread_id::get() as u64;

    if unlikely(tid == INVALID_TID) {
        // we can't have any thread id be our INVALID_TID
        abort();
    }

    tid
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
        unsafe { self.alloc_ptr.as_ref() }
    }

    #[inline]
    fn as_mut(&mut self) -> &mut T {
        // SAFETY: This is safe because we know that alloc_ptr can't be zero
        // and because we know that alloc_ptr has to point to a valid
        // instance of T in memory
        unsafe { self.alloc_ptr.as_mut() }
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

lazy_static! {
    static ref GUARDS: ThreadLocal<AtomicPtr<()>> = ThreadLocal::new();
}
