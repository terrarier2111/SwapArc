use crossbeam_utils::{Backoff, CachePadded};
use std::alloc::{alloc, dealloc, Layout, LayoutError};
use std::cell::{Cell, UnsafeCell};
use std::mem::{align_of, size_of, ManuallyDrop, transmute, MaybeUninit};
use std::ops::Deref;
use std::process::abort;
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering, AtomicU8, AtomicU64};
use std::{mem, ptr, thread};
use std::hash::BuildHasherDefault;
use std::sync::Mutex;
use likely_stable::{likely, unlikely};
use rustc_hash::{FxHasher, FxHashSet};
use thread_local::{ThreadLocal, UnsafeToken};
use crate::TID;

const DEBUG: bool = false;

// Debt(t) <= Debt(t + 1)
// Refs >= Debt
// Refs(t + 1) >= Debt(t)

// FIXME: currently there is a bug where the thread_local would sometimes tell callers that
// FIXME: some token isn't used yet when it clearly is used.

// FIXME: note that sometimes, there is some inconsistency in the debt vs ref cnt at the end (the debt is too large)
// FIXME: but if the counts match, the detached flag isn't set

// FIXME: for some reason the tid in the metadata is oftentimes just INVALID_TID
// FIXME: this is especially the case at the beginning of a thread local if an entry is an alternative entry and no "normal" entry
// FIXME: furthermore in this case the alternative entry always (for all `rc`s starting with 0) has the same stripped debt as its rc
// FIXME: but the debt has its flag set and as such the values are not actually completely the same.
// FIXME: also the master entry of the returned alternative entry usually gets cleaned up before the alternative entry gets acquired.

// FIXME: for the other time when the program doesn't finish, there's a couple of "failed to swap TID"-messages floating around
// FIXME: and the issue is that the expected tid is equal to the current tid but the cache's tid is not the same as the current tid
// FIXME: and as such the comparison fails (for detached caches)

// FIXME: but sometimes the hang even happens without any "failed to swap TID"-messages.


// FIXME: all instances of tid and cache.thread_id differing are in alternative caches!

pub struct AutoLocalArc<T: Send + Sync> {
    inner: NonNull<InnerArc<T>>,
    cache: UnsafeCell<NonNull<Cache<T>>>,
}

unsafe impl<T: Send + Sync> Send for AutoLocalArc<T> {}
unsafe impl<T: Send + Sync> Sync for AutoLocalArc<T> {}

impl<T: Send + Sync> AutoLocalArc<T> {
    #[no_mangle]
    pub fn new(val: T) -> Self {
        let inner = SizedBox::new(InnerArc {
            val,
            cache: Default::default(),
        })
        .into_ptr();
        let tid = thread_id();
        let cache = unsafe { inner.as_ref()
            .cache
            .get_or(|token| {
                // println!("inserting: {}", thread_id());
                DetachablePtr(
                   Some(token)
                )
            }, |token| {
                if DEBUG {
                    println!("write initial cache: {:?}", unsafe { transmute::<_, *const ()>(token) });
                }
                unsafe { &mut *token.meta().cache.get() }.write(Cache {
                    parent: inner,
                    src: AtomicPtr::new(null_mut()), // we have no src, as we are the src ourselves
                    thread_id: tid,
                    token: token.clone().into_unsafe_token(),
                });
                token.meta().ref_cnt.store(1, Ordering::Release);
                token.meta().thread_id.store(tid, Ordering::Release);
            }).meta().cache.get().as_ref().unwrap_unchecked().as_ptr() };
        if DEBUG {
            println!("initial: tid {} address {:?}", tid, cache);
        }
        let ret = Self {
            inner,
            cache: UnsafeCell::new(unsafe { NonNull::new_unchecked(cache.cast_mut()) }),
        };
        ret
    }

    #[inline(always)]
    fn inner(&self) -> &InnerArc<T> {
        unsafe { self.inner.as_ref() }
    }
}

/// We have to choose a lower highest ref cnt because we are using the last bit to store metadata
const MAX_REFCOUNT: usize = usize::MAX / 2_usize;

impl<T: Send + Sync> Clone for AutoLocalArc<T> {
    #[inline]
    fn clone(&self) -> Self {
        let cache_ptr = unsafe { *self.cache.get() };
        let cache = unsafe { cache_ptr.as_ref() };
        // check if we are the owner of this alarc's cache

        let tid = thread_id();

        let cache = if cache.thread_id == tid {
            let meta = unsafe { cache.token.meta() };
            // println!("increment local cnt");
            // we are the owner of this cache, just perform a simple increment

            let ref_cnt = meta.ref_cnt.load(Ordering::Relaxed/*Acquire*/);
            if unlikely(ref_cnt >= MAX_REFCOUNT) {
                unsafe {
                    handle_large_ref_count((cache as *const Cache<T>).cast_mut(), ref_cnt);
                }
            }
            meta.ref_cnt.store(ref_cnt + 1, Ordering::Release);
            cache_ptr
        } else {
            // println!("increment non-local cnt");
            // we aren't the owner, now we have to do some more work

            let inner = self.inner;
            let cached = unsafe { self
                .inner()
                .cache
                .get_or(|token| {
                    DetachablePtr(
                        Some(token)
                    )
                }, |token| {
                    if DEBUG {
                        println!("write cache: {:?}", unsafe { transmute::<_, *const ()>(token) });
                    }
                    unsafe { &mut *token.meta().cache.get() }.write(Cache {
                        parent: inner,
                        src: AtomicPtr::new(cache_ptr.as_ptr()),
                        thread_id: tid,
                        token: token.into_unsafe_token(),
                    });
                }) };
            let local_meta = cached.meta();
            let local_cache_ptr = unsafe { NonNull::new_unchecked((&*local_meta.cache.get()).as_ptr().cast_mut()) };
            let local_cache = unsafe { local_cache_ptr.as_ref() };
            // the ref count here can't change, as we are the only thread which is able to access it.
            let ref_cnt = local_meta.ref_cnt.load(Ordering::Relaxed/*Acquire*/);
            let debt = local_meta.debt.load(Ordering::Acquire);

            if ref_cnt != debt && ref_cnt == strip_flags(debt) {
                // FIXME: this actually still happens, why?
                if DEBUG {
                    println!("weird state rc {} | debt {} | stripped {}", ref_cnt, debt, strip_flags(debt));
                }
            }

            // check if the local cache's instance is still valid.
            // check if the local cache is unused, this is the case if either
            // there are no outstanding references to the cache or it was just newly created.
            // we strip the `detached` flag here as it may still be set from a previous cache
            // and as such even though it is not relevant anymore it is still present on the `debt`.
            // FIXME: why is the flag mostly there on alternative entries but not on other entries?

            if ref_cnt == strip_flags(debt) {
                if debt > 0 {
                    // we have a retry loop here in case the debt's updater hasn't finished yet
                    let backoff = Backoff::new();
                    // wait for the other thread to clean up
                    while local_meta.thread_id.load(Ordering::Acquire) != INVALID_TID {
                        backoff.snooze();
                    }
                }
                // the local cache has no valid references anymore
                local_cache.src.store(cache_ptr.as_ptr(), Ordering::Release);
                                                                     // this is `2` because we need a reference for the current thread's instance and
                                                                     // because the newly created instance needs a reference as well.
                local_meta.ref_cnt.store(2, Ordering::Release);
                // we update the debt after the ref_cnt because that enables us to see the `ref_cnt` update by loading `debt` using `Acquire`
                local_meta.debt.store(0, Ordering::Release);
                local_meta.thread_id.store(tid, Ordering::Release);
                local_meta.state.store(CACHE_STATE_IDLE, Ordering::Release);
                // remove the reference to the external cache, as we moved it to our own cache instead
                *unsafe { &mut *self.cache.get() } = local_cache_ptr;
            } else {
                if local_cache.thread_id != tid {
                    if DEBUG {
                        println!("differing ids: cache {:?} cache_tid {} curr {}", local_cache as *const _, local_cache.thread_id, tid);
                    }
                }
                // the local cache is still in use, try simply incrementing its counter
                // if we fail, just fall back to the slow path
                if unlikely(ref_cnt >= MAX_REFCOUNT) {
                    unsafe {
                        handle_large_ref_count((local_cache as *const Cache<T>).cast_mut(), ref_cnt);
                    }
                }
                local_meta.ref_cnt.store(ref_cnt + 1, Ordering::Release);
                // sync with the local store
                fence(Ordering::Acquire); // FIXME: is this even required?
                // FIXME: here is probably a possibility for a race condition (what if we have one of the instances on another thread and it gets freed right
                // FIXME: after we check if ref_cnt == debt?) this can probably be fixed by checking again right after increasing the ref_count and
                // FIXME: handling failure by doing what we would have done if the local cache had no valid references to begin with.
                let debt = local_meta.debt.load(Ordering::Acquire);
                // we don't need to strip the flags here as we know that `debt` isn't detached.
                if debt == ref_cnt {
                    if debt > 0 {
                        // we have a retry loop here in case the debt's updater hasn't finished yet
                        let backoff = Backoff::new();
                        // wait for the other thread to clean up
                        while local_meta.thread_id.load(Ordering::Acquire) != INVALID_TID {
                            backoff.snooze();
                        }
                    }
                    // the local cache has no valid references anymore
                    local_cache.src.store(cache_ptr.as_ptr(), Ordering::Release);
                                                                         // this is `2` because we need a reference for the current thread's instance and
                                                                         // because the newly created instance needs a reference as well.
                    local_meta.ref_cnt.store(2, Ordering::Release);
                    // we update the debt after the ref_cnt because that enables us to see the `ref_cnt` update by loading `debt` using `Acquire`
                    local_meta.debt.store(0, Ordering::Release);
                    local_meta.thread_id.store(tid, Ordering::Release);
                    local_meta.state.store(CACHE_STATE_IDLE, Ordering::Release);
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
#[inline(never)]
unsafe fn handle_large_ref_count<T: Send + Sync>(cache: *mut Cache<T>, ref_cnt: usize) {
    let meta = unsafe { cache.as_ref().unwrap_unchecked().token.meta() };

    // try to recover by decrementing the `debt` count from the `ref_cnt` and setting the `debt` count to 0
    // note: the `detached` flag can't be set because this method is only called with the `local_cache` as the `cache` parameter
    let debt = meta.debt.swap(0, Ordering::AcqRel); // FIXME: can we use a weaker ordering?

    if debt == 0 {
        // ran out of references
        abort();
    }

    let ref_cnt = ref_cnt - debt;

    meta.ref_cnt.store(ref_cnt, Ordering::Release);
    abort(); // FIXME: get rid of this once the testing phase is over!
}

impl<T: Send + Sync> Drop for AutoLocalArc<T> {
    #[inline]
    fn drop(
        &mut self) {
        let cache_ptr = unsafe { *self.cache.get() };
        let cache = unsafe { cache_ptr.as_ref() };
        let meta = unsafe { cache.token.meta() };
        let tid = thread_id();
        if cache.thread_id == tid {
            let ref_cnt = meta.ref_cnt.load(Ordering::Relaxed);
            meta.state.store(CACHE_STATE_IN_USE, Ordering::Release);
            meta.ref_cnt.store(ref_cnt - 1, Ordering::Release);
            let ref_cnt = ref_cnt - 1;
            // synchronize with ref_cnt in order for future loads to occur after the local store.
            fence(Ordering::Acquire);
            // `ref_cnt` can't race with `debt` here because we are the only ones who are able to update `ref_cnt`
            let debt = meta.debt.load(Ordering::Acquire);
            if DEBUG {
                if tid == TID.load(Ordering::Acquire) as u64 {
                    println!(
                        "local drop: {} refs: {} debt: {}",
                        cache.thread_id, ref_cnt, debt
                    );
                }
            }
            // TODO: we could add a fastpath here for debt == 0
            if ref_cnt == strip_flags(debt) {
                if DEBUG {
                    println!("rc is debt: {}", is_detached(debt));
                }
                if cleanup_cache::<true, T>(cache_ptr, is_detached(debt), tid) { // FIXME: we don't need to check for an expected TID here.
                    // don't modify the state anymore as it might have been deallocated or be reassociated with another
                    // thread's entry
                    return;
                }
            }
            // signal that we are done using the cache.
            meta.state.store(CACHE_STATE_IDLE, Ordering::Release);
        } else {
            // println!("non-local cache ptr drop!");
            let guard = unsafe { guard().as_ref().unwrap_unchecked() };
            guard.store(cache_ptr.as_ptr().cast(), Ordering::Release);
            let cache_tid = cache.thread_id;
            let debt = meta.debt.fetch_add(1, Ordering::AcqRel) + 1;
            fence(Ordering::Acquire);
            let ref_cnt = meta.ref_cnt.load(Ordering::Acquire);
            if DEBUG {
                if tid == TID.load(Ordering::Acquire) as u64 {
                    println!(
                        "FAILED local drop: {} refs: {} debt: {} | glob {} | tid {} address {:?}",
                        cache_tid, ref_cnt, debt/*, fin*/, TID.load(Ordering::Acquire), tid, cache_ptr.as_ptr()
                    );
                }
            }
            if strip_flags(debt) == ref_cnt {
                if DEBUG {
                    println!("rc is debt external curr {} | cached_id {} | debt {} | stripped {} | rc {}", tid, cache_tid, debt, strip_flags(debt), ref_cnt);
                }
                // FIXME: NEW: there is probably a race condition here - what if cache's owner increases its reference count here
                // FIXME: and we free its cache just down below?

                // there are no external refs alive
                cleanup_cache::<true, T>(cache_ptr, is_detached(debt), cache_tid);
            }
            guard.store(sentinel_addr().cast_mut(), Ordering::Release);
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
    cache: ThreadLocal<DetachablePtr<T>, Metadata<T>, false>,
}

impl<T: Send + Sync> InnerArc<T> {

    #[inline]
    fn prepare_drop(&self) {
        // wait for all non-local cache users to finish
        for cache in GUARDS.lock().unwrap().iter() {
            let cache = unsafe { (*cache as *const AtomicPtr<()>).as_ref().unwrap_unchecked() };
            let backoff = Backoff::new();
            while cache.load(Ordering::Acquire).cast_const() == (self as *const InnerArc<T>).cast() {
                backoff.snooze();
            }
        }

        // wait for all local cache users to finish
        for cache in self.cache.iter() {
            let backoff = Backoff::new();
            // wait until the local thread is done using its cache
            let state = &unsafe { cache.meta() }.state;
            while state.load(Ordering::Acquire) != CACHE_STATE_IDLE {
                backoff.snooze();
            }
            // signal to the destructor that we are finished.
            state.store(CACHE_STATE_FINISH, Ordering::Release);
        }
    }

}

impl<T: Send + Sync> Drop for InnerArc<T> {
    #[inline]
    fn drop(&mut self) {
        for mut entry in self.cache.iter_mut() {
            *entry.value_mut() = DetachablePtr(None);
        }
    }
}

#[repr(C)]
struct Cache<T: Send + Sync> {
    parent: NonNull<InnerArc<T>>,
    thread_id: u64,
    token: UnsafeToken<DetachablePtr<T>, Metadata<T>, false>,
    src: AtomicPtr<Cache<T>>,
}

// This gets called when the debt and the ref_cnt in a cache are the same
// and thus we know no outstanding references (neither local nor external)
// to the cache are left and we can clean up the cache without issues.
fn cleanup_cache<const UPDATE_SUPER: bool, T: Send + Sync>(
    cache: NonNull<Cache<T>>,
    detached: bool,
    exp_tid: u64,
) -> bool {
    let token = unsafe { cache.as_ref().token.duplicate() };
    let meta = unsafe { token.meta() };

    if UPDATE_SUPER {
        match meta.thread_id.compare_exchange(exp_tid, INVALID_TID, Ordering::AcqRel, Ordering::Relaxed) { // FIXME: can we get tid of this (at least in some cases)
            Ok(_) => {
                // FIXME: when freeing the own memory, we still have a reference to it and thus UB
                // FIXME: the same thing is the case when freeing the major allocation
                // FIXME: check for `detached` and deallocate own memory if detached


                // ORDERING: this is Acquire because we have to link to the previous store to `src` which might have
                // happened on a different thread than this (the deallocating) one.
                let src = unsafe { cache.as_ref() }.src.load(Ordering::Acquire);
                if DEBUG {
                    println!("start cleanup {:?} (child of {:?}) token {:?}", cache.as_ptr(), src, unsafe { transmute::<_, *const ()>(token.duplicate()) });
                }
                // there are no external refs alive
                if src.is_null() {
                    if DEBUG {
                        println!("major cleanup!");
                    }
                    meta
                        .debt
                        .fetch_or(DETACHED_FLAG, Ordering::AcqRel);
                    // signal that we are finished so we don't accidentally loop indefinitely in the `InnerArc`
                    // destructor as we don't reach our update here "normally".
                    meta.state.store(CACHE_STATE_IDLE, Ordering::Release);
                    fence(Ordering::Acquire);

                    // we are the first "(cache) node", so we need to free the memory
                    unsafe {
                        drop_slow::<T>(unsafe { cache.as_ref() }.parent);
                    }

                    #[cold]
                    unsafe fn drop_slow<T: Send + Sync>(ptr: NonNull<InnerArc<T>>) {
                        if DEBUG {
                            println!("start cleanup main!");
                        }
                        unsafe { ptr.as_ref() }.prepare_drop();
                        if DEBUG {
                            println!("cleanup main!");
                        }

                        drop(unsafe { SizedBox::from_ptr(ptr) });
                        if DEBUG {
                            println!("cleaned up main!");
                        }
                    }
                    return true;
                } else {
                    // FIXME: we have other caches above us, so release a reference to them - this has to happen recursively tho as they could have to release their last reference as well.
                    // FIXME: this should work - now make it iterative instead of recursive!

                    let super_cache_ptr = unsafe { NonNull::new_unchecked(src) };
                    let super_cache = unsafe { super_cache_ptr.as_ref() };
                    let super_meta = unsafe { super_cache.token.meta() };
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
                    if strip_flags(debt) == ref_cnt {
                        if DEBUG {
                            println!("cleanup other!");
                        }

                        fence(Ordering::AcqRel);
                        cleanup_cache::<true, T>(super_cache_ptr, super_detached, super_tid);
                        // fence(Ordering::AcqRel);
                    }
                }
            }
            Err(err) => {
                if DEBUG {
                    println!("failed to swap TID {} exp {} inval {} cache {:?}", err, exp_tid, usize::MAX, cache.as_ptr());
                }
            }
        }
    }
    if detached {
        // ORDERING: We use a fence here in order to ensure that all other operations and uses
        // of atomics happen-before this.
        fence(Ordering::Acquire);
        if DEBUG {
            println!("destructed {:?}", cache.as_ptr());
        }
        unsafe { token.destruct(); }
    }
    detached
}

struct Metadata<T: Send + Sync> {
    thread_id: AtomicU64,
    ref_cnt: AtomicUsize, // this ref cnt may only be updated by the current thread
    debt: AtomicUsize, // this debt count may only be updated by threads other than the current one - this has a `detached` flag as its last bit
    state: AtomicU8,
    cache: UnsafeCell<MaybeUninit<Cache<T>>>,
}

unsafe impl<T: Send + Sync> Send for Metadata<T> {}
unsafe impl<T: Send + Sync> Sync for Metadata<T> {}

impl<T: Send + Sync> Default for Metadata<T> {
    fn default() -> Self {
        Self {
            thread_id: Default::default(),
            ref_cnt: Default::default(),
            debt: Default::default(),
            state: Default::default(),
            cache: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

const CACHE_STATE_IDLE: u8 = 0;
const CACHE_STATE_IN_USE: u8 = 1;
const CACHE_STATE_FINISH: u8 = 2;

unsafe impl<T: Send + Sync> Send for Cache<T> {}
unsafe impl<T: Send + Sync> Sync for Cache<T> {}

#[repr(transparent)]
struct DetachablePtr<T: Send + Sync>(Option<UnsafeToken<DetachablePtr<T>, Metadata<T>, false>>);

unsafe impl<T: Send + Sync> Send for DetachablePtr<T> {}
unsafe impl<T: Send + Sync> Sync for DetachablePtr<T> {}

impl<T: Send + Sync> Drop for DetachablePtr<T> {
    #[inline]
    fn drop(&mut self) {
        if let Some(entry) = self.0.as_ref() {
            let meta = unsafe { entry.meta() };

            if meta.state.load(Ordering::Acquire) != CACHE_STATE_FINISH {
                let cache = unsafe { (&*meta.cache.get()).assume_init_ref() };

                // ORDERING: This is `Acquire` as the dropping thread
                // doesn't necessarily have to be the same as the one
                // owning the cache and we have to make sure that all decrements happened before this.
                let ref_cnt = meta.ref_cnt.load(Ordering::Acquire);
                let tid = cache.thread_id;
                // mark the cache as detached in order to allow other threads to see
                if DEBUG {
                    println!("detached {:?}", cache as *const Cache<T>);
                }
                let debt = meta.debt.fetch_or(DETACHED_FLAG, Ordering::AcqRel); // TODO: try avoiding having to do this RMW for most cases by adding a fast path
                // FIXME: are we sure that ref_cnt's last desired value is already visible to us here?
                if debt == ref_cnt {
                    // we don't have to wait for other threads to finish as they will access
                    // the cache which is available as long as there are threads accessing the
                    // central data structure and not the backing allocation which can get deallocated
                    // at any point in time.
                    fence(Ordering::AcqRel);
                    if debt == 0 {
                        if DEBUG {
                            println!("zero drop!");
                        }
                    }
                    cleanup_cache::<false, T>(unsafe { NonNull::new_unchecked((cache as *const Cache<T>).cast_mut()) }, true, tid); // FIXME: do we have to set a marker here?
                }
            }
        }
    }
}

type TLocal<T> = ThreadLocal<DetachablePtr<T>, Metadata<T>, false>;

const INVALID_TID: u64 = u64::MAX - 1;
const NOT_PRESENT: u64 = u64::MAX;

#[thread_local]
static ACTUAL_TID: Cell<u64> = Cell::new(NOT_PRESENT);

#[inline]
fn thread_id() -> u64 {
    let act_tid = ACTUAL_TID.get();
    if unlikely(act_tid == NOT_PRESENT) {
        let tid = set_actual_tid();

        return tid;
    }
    act_tid
}

#[cold]
#[inline(never)]
fn set_actual_tid() -> u64 {
    // let tid = thread::current().id().as_u64().get();
    let tid = thread_id::get() as u64;

    if unlikely(tid >= INVALID_TID) {
        // we can't have any thread id be our INVALID_TID
        abort();
    }

    ACTUAL_TID.set(tid);

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
        if alloc.is_null() {
            panic!("Out of memory!");
        }
        // FIXME: add safety comment
        unsafe {
            alloc.write(val);
        }
        Self {
            alloc_ptr: unsafe { NonNull::new_unchecked(alloc) },
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

// The sentinel here is used to signal to other threads that there is no cache
// being modified by a thread (in their guards) at the moment. But because we
// need some address, we need a static piece of memory with a unique address.
static SENTINEL: u8 = 0;

#[inline(always)]
fn sentinel_addr() -> *const () {
    (&SENTINEL as *const u8).cast()
}

static GUARDS: Mutex<FxHashSet<usize>> = Mutex::new(FxHashSet::with_hasher({
    if size_of::<BuildHasherDefault<FxHasher>>() != size_of::<()>() || align_of::<BuildHasherDefault<FxHasher>>() != align_of::<()>() {
        panic!("Unexpected layout of BuildHasherDefault");
    }
    unsafe { transmute::<(), BuildHasherDefault<FxHasher>>(()) }})); // FIXME: this is unsound, use const constructor, once its available!

#[thread_local]
static GUARD: AtomicPtr<()> = AtomicPtr::new(null_mut());
thread_local! { static DROP_GUARD: DropGuard = const { DropGuard }; }

struct DropGuard;

impl Drop for DropGuard {
    fn drop(&mut self) {
        GUARDS.lock().unwrap().remove(&((&GUARD as *const AtomicPtr<()>) as usize));
    }
}

// This method may only be called locally.
fn guard() -> *const AtomicPtr<()> {
    if GUARD.load(Ordering::Relaxed) == null_mut() {
        // initialize guard
        DROP_GUARD.with(|_| {});

        GUARD.store(sentinel_addr().cast_mut(), Ordering::Relaxed);
        GUARDS.lock().unwrap().insert((&GUARD as *const AtomicPtr<()>) as usize);
    }
    &GUARD as *const _
}
