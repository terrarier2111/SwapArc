use crossbeam_utils::CachePadded;
use std::alloc::{alloc, dealloc, Layout, LayoutError};
use std::cell::UnsafeCell;
use std::mem::{align_of, size_of, ManuallyDrop};
use std::ops::Deref;
use std::process::abort;
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering};
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
            dropping: AtomicBool::new(false),
        })
        .into_ptr()
        .as_ptr();
        let cache = unsafe { &*inner }
            .cache
            .get_or(|| {
                // println!("inserting: {}", thread_id());
                DetachablePtr(
                    SizedBox::new(CachePadded::new(Cache {
                        parent: inner,
                        ref_cnt: AtomicUsize::new(1),
                        debt: AtomicUsize::new(0),
                        thread_id: thread_id(),
                        src: AtomicPtr::new(null_mut()), // we have no src, as we are the src ourselves
                        finish_cnt: AtomicUsize::new(0),
                    }))
                    .into_ptr()
                    .as_ptr(),
                )
            })
            .0;
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

/// We have to choose a lower highest ref cnt than the std lib as we are using the last bit to store metadata
const MAX_REFCOUNT: usize = MAX_HARD_REFCOUNT / 2;
const MAX_HARD_REFCOUNT: usize = (isize::MAX) as usize / 2;

impl<T: Send + Sync> Clone for AutoLocalArc<T> {
    #[inline]
    fn clone(&self) -> Self {
        let cache_ptr = unsafe { *self.cache.get() };
        let cache = unsafe { &*cache_ptr };
        let cache = if cache.thread_id == thread_id() {
            // this is race-free!
            let ref_cnt = cache.ref_cnt.load(Ordering::Relaxed/*Acquire*/);
            if ref_cnt > MAX_REFCOUNT {
                unsafe {
                    handle_large_ref_count(cache, ref_cnt);
                }
            }
            cache.ref_cnt.store(ref_cnt + 1, Ordering::Release);
            cache_ptr
        } else {
            let inner = self.inner;
            let local_cache_ptr = self
                .inner()
                .cache
                .get_or(|| {
                    println!("inserting: {}", thread_id());
                    DetachablePtr(
                        SizedBox::new(CachePadded::new(Cache {
                            parent: inner,
                            src: AtomicPtr::new(cache_ptr),
                            debt: AtomicUsize::new(0),
                            thread_id: thread_id(),
                            ref_cnt: AtomicUsize::new(0),
                            finish_cnt: AtomicUsize::new(0),
                        }))
                        .into_ptr()
                        .as_ptr(),
                    )
                })
                .0;
            let local_cache = unsafe { &*local_cache_ptr };
            // the ref count here can't change, as we are the only thread which is able to access it.
            let ref_cnt = local_cache.ref_cnt.load(Ordering::Relaxed/*Acquire*/);
            let debt = local_cache.debt.load(Ordering::Acquire);
            if ref_cnt == debt {
                if debt > 0 {
                    // we have a retry loop here in case the debt's updater hasn't finished yet
                    let mut finish_cnt = local_cache.wait_for::<false>(ref_cnt);
                    if !is_deleted(finish_cnt) {
                        println!("battle race!");
                        // the other thread didn't see our increment in time, so we have to wait!
                        cleanup_cache::<true, T>(local_cache_ptr, local_cache.guard(), false);
                    }
                }
                // the local cache has no valid references anymore
                local_cache.src.store(cache_ptr, Ordering::Relaxed/*Release*/); // FIXME: can the lack of synchronization between these three updates lead to race conditions?
                                                                     // this is `2` because we need a reference for the current thread's instance and
                                                                     // because the newly created instance needs a reference as well.
                local_cache.ref_cnt.store(2, Ordering::Release);
                // we update the debt after the ref_cnt because that enables us to see the `ref_cnt` update by loading `debt` using `Acquire`
                local_cache.debt.store(0, Ordering::Release);
                local_cache.finish_cnt.store(0, Ordering::Release);
                // remove the reference to the external cache, as we moved it to our own cache instead
                *unsafe { &mut *self.cache.get() } = local_cache_ptr;
            } else {
                // the local cache is still in use
                if ref_cnt > MAX_REFCOUNT {
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
                        if !is_deleted(finish_cnt) {
                            println!("battle race!");
                            // the other thread didn't see our increment in time, so we have to wait!
                            cleanup_cache::<true, T>(local_cache_ptr, local_cache.guard(), false);
                        }
                    }
                    println!("racing load!");
                    // the local cache has no valid references anymore
                    local_cache.src.store(cache_ptr, Ordering::Relaxed/*Release*/); // FIXME: can the lack of synchronization between these three updates lead to race conditions?
                                                                         // this is `2` because we need a reference for the current thread's instance and
                                                                         // because the newly created instance needs a reference as well.
                    local_cache.ref_cnt.store(2, Ordering::Release);
                    // we update the debt after the ref_cnt because that enables us to see the `ref_cnt` update by loading `debt` using `Acquire`
                    local_cache.debt.store(0, Ordering::Release);
                    local_cache.finish_cnt.store(0, Ordering::Release);
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
        if cache.thread_id == thread_id() {
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
                if !is_deleted(finish_cnt) {
                    // we know that the thread we waited on freed the reference
                    let guard = cache.guard();
                    drop(cache);
                    // there are no external refs alive
                    cleanup_cache::<true, T>(cache_ptr, guard, is_detached(debt));
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
                let _finish_cnt = cache.wait_for::<true>(ref_cnt);
                let guard = cache.guard();
                drop(cache);
                fence(Ordering::AcqRel);
                // there are no external refs alive
                if !cleanup_cache::<true, T>(cache_ptr, guard, is_detached(debt)) {
                    fence(Ordering::AcqRel);
                    cache
                        .finish_cnt
                        .store(ref_cnt | DELETION_FLAG, Ordering::Release);
                }
            } else {
                fence(Ordering::AcqRel);
                cache.finish_cnt.fetch_add(1, Ordering::AcqRel);
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
    dropping: AtomicBool,
}

impl<T: Send + Sync> Drop for InnerArc<T> {
    #[inline]
    fn drop(&mut self) {
        self.dropping.store(true, Ordering::Release);
        // make sure all usages of the internal value happen before this
        fence(Ordering::Acquire);
    }
}

#[repr(C)]
struct Cache<T: Send + Sync> {
    parent: *const InnerArc<T>,
    thread_id: u64,
    src: AtomicPtr<CachePadded<Cache<T>>>,
    debt: AtomicUsize, // this debt count may only be updated by threads other than the current one - this has a `detached` flag as its last bit
    ref_cnt: AtomicUsize, // this ref cnt may only be updated by the current thread
    finish_cnt: AtomicUsize,
}

impl<T: Send + Sync> Cache<T> {
    #[inline(always)]
    fn guard(&self) -> *const AtomicBool {
        unsafe {
            self.parent
                .byte_add(memoffset::offset_of!(InnerArc<T>, dropping))
                .cast::<AtomicBool>()
        }
    }

    fn wait_for<const OWNS_LOCAL_REF: bool>(&self, ref_cnt: usize) -> usize {
        let mut finish_cnt = self.finish_cnt.load(Ordering::Acquire);
        let local_add = if OWNS_LOCAL_REF { 1 } else { 0 };
        while ref_cnt != strip_flags(finish_cnt) + local_add {
            finish_cnt = self.finish_cnt.load(Ordering::Acquire);
        }
        finish_cnt
    }
}

fn cleanup_cache<const UPDATE_SUPER: bool, T: Send + Sync>(
    cache: *const CachePadded<Cache<T>>,
    dropping_guard: *const AtomicBool,
    detached: bool,
) -> bool {
    if UPDATE_SUPER {
        // FIXME: when freeing the own memory, we still have a reference to it and thus UB
        // FIXME: the same thing is the case when freeing the major allocation
        // println!("cleanup cache: {}", unsafe { &*cache }.ref_cnt.load(Ordering::Relaxed));
        // FIXME: check for `detached` and deallocate own memory if detached
        let src = unsafe { &*cache }.src.load(Ordering::Relaxed/*Acquire*/);
        // there are no external refs alive
        if src.is_null() {
            println!("delete main thingy: {}", thread_id());
            fence(Ordering::AcqRel); // FIXME: make sure this fence is coupled with some other atomic operation!
            unsafe { &*cache }
                .debt
                .fetch_or(DETACHED_FLAG, Ordering::AcqRel);
            fence(Ordering::AcqRel); // FIXME: make sure this fence is coupled with some other atomic operation!
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
            if !unsafe { &*dropping_guard }.load(Ordering::Acquire) {
                let super_cache_ptr = src;
                let super_cache = unsafe { &*super_cache_ptr };
                let debt = super_cache.debt.fetch_add(1, Ordering::AcqRel) + 1; // we add 1 at the end to make sure we use the post-update value
                // this fence protects the following load of `ref_cnt` from being moved before the guard increment.
                fence(Ordering::Acquire);
                // when `DetachablePtr` gets dropped, it will check if it has to free the allocation itself and thus
                // if we observe that `DetachablePtr` was dropped and because we are still alive it didn't free the allocation,
                // we know that we have to free the allocation ourselves.
                let super_detached = is_detached(debt);
                let ref_cnt = super_cache.ref_cnt.load(Ordering::Acquire);
                if strip_flags(debt) == ref_cnt {
                    // we have a retry loop here in case the debt's updater hasn't finished yet
                    let _finish_cnt = super_cache.wait_for::<true>(ref_cnt); // FIXME: can a data race occur here?
                    // fence(Ordering::AcqRel);

                    drop(super_cache);

                    fence(Ordering::AcqRel);
                    if !cleanup_cache::<true, T>(super_cache_ptr, dropping_guard, super_detached) {
                        // fence(Ordering::AcqRel);
                        super_cache
                            .finish_cnt
                            .store(strip_flags(debt) | DELETION_FLAG, Ordering::Release);
                    }
                    // fence(Ordering::AcqRel);
                } else {
                    // println!("other add from thread {} for thread {} refs {} debt {}", thread_id(), super_cache.thread_id, ref_cnt, debt);
                    // println!("other add for thread {} refs {} debt {}", super_cache.thread_id, ref_cnt, debt);
                    // println!("other add refs {} debt {}", ref_cnt, debt);
                    fence(Ordering::AcqRel);
                    super_cache.finish_cnt.fetch_add(1, Ordering::AcqRel);
                    // fence(Ordering::AcqRel);
                }
            }
        }
    }
    if detached {
        fence(Ordering::AcqRel); // FIXME: make sure this fence is coupled with some other atomic operation!
        unsafe { SizedBox::from_ptr(NonNull::new_unchecked(cache.cast_mut())) };
        return true;
    }
    false
}

// unsafe impl<T: Send> Send for Cache<T> {}
// unsafe impl<T: Sync> Sync for Cache<T> {}
unsafe impl<T: Send + Sync> Send for Cache<T> {}
unsafe impl<T: Send + Sync> Sync for Cache<T> {}

#[derive(Clone)]
#[repr(transparent)]
struct DetachablePtr<T: Send + Sync>(*mut CachePadded<Cache<T>>);

/*
impl<T: Send + Sync> DetachablePtr<T> {

    #[inline]
    fn detach(self) {
        unsafe { &*self.0 }.detached.store(true, Ordering::Relaxed);
    }

}*/

impl<T: Send + Sync> Drop for DetachablePtr<T> {
    #[inline]
    fn drop(&mut self) {
        let cache = unsafe { &*self.0 };
        // println!("dropping detachable ptr: {}", cache.thread_id);
        // if !is_detached(cache.debt.load(Ordering::Acquire)) {
        if !unsafe { &*cache.guard() }.load(Ordering::Acquire) {
            // println!("dropping detachable ptr!");
            // unsafe { &*self.0 }.detached.store(true, Ordering::Relaxed);
            let ref_cnt = cache.ref_cnt.load(Ordering::Relaxed/*Acquire*/);
            let debt = cache.debt.fetch_or(DETACHED_FLAG, Ordering::AcqRel);
            // here no race condition can occur because we are the only ones who can update `ref_cnt` // FIXME: but can `debt`'s orderings still make data races possible?
            if debt == ref_cnt {
                // fence(Ordering::AcqRel);
                // we have a retry loop here in case the debt's updater hasn't finished yet
                cache.wait_for::<false>(ref_cnt);
                let guard = cache.guard();
                let cache_ptr = self.0;
                drop(cache);
                fence(Ordering::AcqRel);
                cleanup_cache::<false, T>(cache_ptr, guard, true);
            }
        } else {
            drop(cache);
            // we are being dropped after the main struct got dropped, clean up!
            unsafe { SizedBox::from_ptr(NonNull::new_unchecked(self.0)) };
        }
    }
}

unsafe impl<T: Send + Sync> Send for DetachablePtr<T> {}

#[inline]
fn thread_id() -> u64 {
    // thread::current().id().as_u64().get()
    thread_id::get() as u64
}

const DETACHED_FLAG: usize = 1 << (usize::BITS - 1); // this is for `debt`
const DELETION_FLAG: usize = 1 << (usize::BITS - 1); // this is for `finish_cnt`

#[inline(always)]
const fn strip_flags(debt: usize) -> usize {
    debt & !(DETACHED_FLAG | DELETION_FLAG)
}

#[inline(always)]
const fn is_detached(debt: usize) -> bool {
    debt & DETACHED_FLAG != 0
}

#[inline(always)]
const fn is_deleted(finish_cnt: usize) -> bool {
    finish_cnt & DELETION_FLAG != 0
}

struct SizedBox<T> {
    alloc_ptr: NonNull<T>,
}

impl<T> SizedBox<T> {
    const LAYOUT: Layout = {
        match Layout::from_size_align(size_of::<T>(), align_of::<T>()) {
            Ok(layout) => layout,
            Err(_) => panic!("Layout error!"),
        }
    }; // FIXME: can we somehow retain the error message?

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
