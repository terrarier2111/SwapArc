use std::cell::UnsafeCell;
use std::mem::{align_of, ManuallyDrop, size_of};
use std::ops::Deref;
use std::process::abort;
use std::{mem, ptr, thread};
use std::alloc::{alloc, dealloc, Layout, LayoutError};
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, fence, Ordering};
use crossbeam_utils::CachePadded;
use thread_local::ThreadLocal;

pub struct AutoLocalArc<T: Send + Sync> {
    inner: *const InnerCachedArc<T>,
    cache: UnsafeCell<*mut CachePadded<Cache<T>>>,
}

// unsafe impl<T: Send> Send for CachedArc<T> {}
// unsafe impl<T: Sync> Sync for CachedArc<T> {}
unsafe impl<T: Send + Sync> Send for AutoLocalArc<T> {}
unsafe impl<T: Send + Sync> Sync for AutoLocalArc<T> {}

impl<T: Send + Sync> AutoLocalArc<T> {

    pub fn new(val: T) -> Self {
        let inner = SizedBox::new(InnerCachedArc {
            val,
            cache: Default::default(),
            dropping: AtomicBool::new(false),
        }).into_ptr().as_ptr();
        let cache = unsafe { &*inner }.cache.get_or(|| {
            DetachablePtr(SizedBox::new(CachePadded::new(Cache {
                parent: inner,
                ref_cnt: AtomicUsize::new(1),
                debt: Default::default(),
                thread_id: thread_id(),
                src: AtomicPtr::new(null_mut()), // we have no src, as we are the src ourselves
            })).into_ptr().as_ptr())
        }).0;
        let ret = Self {
            inner,
            cache: cache.into(),
        };
        ret
    }

    #[inline(always)]
    fn inner(&self) -> &InnerCachedArc<T> {
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
            let ref_cnt = cache.ref_cnt.load(Ordering::Relaxed);
            if ref_cnt > MAX_REFCOUNT {
                unsafe { handle_large_ref_count(cache, ref_cnt); }
            }
            cache.ref_cnt.store(ref_cnt + 1, Ordering::Relaxed);
            (cache as *const CachePadded<Cache<T>>).cast_mut()
        } else {
            let inner = self.inner;
            let local_cache_ptr = self.inner().cache.get_or(|| {
                DetachablePtr(SizedBox::new(CachePadded::new(Cache {
                    parent: inner,
                    src: AtomicPtr::new((cache as *const CachePadded<Cache<T>>).cast_mut()),
                    debt: Default::default(),
                    thread_id: thread_id(),
                    ref_cnt: AtomicUsize::new(0),
                })).into_ptr().as_ptr())
            }).0;
            let local_cache = unsafe { &*local_cache_ptr };
            let ref_cnt = local_cache.ref_cnt.load(Ordering::Relaxed);
            if ref_cnt == local_cache.debt.load(Ordering::Relaxed) {
                // the local cache has no valid references anymore
                local_cache.src.store(local_cache_ptr, Ordering::Relaxed); // FIXME: can the lack of synchronization between these three updates lead to race conditions?
                // this is `2` because we need a reference for the current thread's instance and
                // because the newly created instance needs a reference as well.
                local_cache.ref_cnt.store(2, Ordering::Relaxed);
                // we update the debt after the ref_cnt because that enables us to see the `ref_cnt` update by loading `debt` using `Acquire`
                local_cache.debt.store(0, Ordering::Release);
                // remove the reference to the external cache, as we moved it to our own cache instead
                *unsafe { &mut *self.cache.get() } = local_cache_ptr;
            } else {
                // the local cache is still in use
                if ref_cnt > MAX_REFCOUNT {
                    unsafe { handle_large_ref_count(local_cache, ref_cnt); }
                }
                local_cache.ref_cnt.store(ref_cnt + 1, Ordering::Relaxed);
                // FIXME: here is probably a possibility for a race condition (what if we have one of the instances on another thread and it gets freed right
                // FIXME: after we check if ref_cnt == debt?) this can probably be fixed by checking again right after increasing the ref_count and
                // FIXME: handling failure by doing what we would have done if the local cache had no valid references to begin with.
                if local_cache.debt.load(Ordering::Relaxed) == ref_cnt {
                    // FIXME: fallback
                    println!("racing load!");
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
        let cache = unsafe { &**self.cache.get() };
        if cache.thread_id == thread_id() {
            let ref_cnt = cache.ref_cnt.load(Ordering::Relaxed);
            // println!("dropping {}", ref_cnt);
            cache.ref_cnt.store(ref_cnt - 1, Ordering::Relaxed);
            let ref_cnt = ref_cnt - 1;
            // `ref_cnt` can't race with `debt` here because we are the only ones who are able to update `ref_cnt`
            let debt = cache.debt.load(Ordering::Relaxed);
            // println!("debt: {}\nref_cnt: {}", strip_flags(debt), ref_cnt);
            if ref_cnt == strip_flags(debt) {
                let guard = cache.guard();
                let cache_ptr = cache as *const _;
                drop(cache);
                // there are no external refs alive
                cleanup_cache(cache_ptr, guard, is_detached(debt));
            }
        } else {
            let debt = strip_flags(cache.debt.fetch_add(1, Ordering::Acquire)) + 1;
            if debt > MAX_HARD_REFCOUNT {
                abort();
            }
            if debt == cache.ref_cnt.load(Ordering::Relaxed) { // FIXME: ref_cnt can probably race with `debt` here
                                                                         // FIXME: so what we need to do is add further validation after this check
                                                                         // FIXME: this is probably in form of HazardPointers or the like
                let guard = cache.guard();
                let cache_ptr = cache as *const _;
                drop(cache);
                // there are no external refs alive
                cleanup_cache(cache_ptr, guard, is_detached(debt));
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
struct InnerCachedArc<T: Send + Sync> {
    val: T,
    cache: ThreadLocal<DetachablePtr<T>>,
    dropping: AtomicBool,
}

impl<T: Send + Sync> Drop for InnerCachedArc<T> {
    #[inline]
    fn drop(&mut self) {
        self.dropping.store(true, Ordering::Relaxed);
    }
}

struct Cache<T: Send + Sync> {
    parent: *const InnerCachedArc<T>,
    thread_id: u64,
    src: AtomicPtr<CachePadded<Cache<T>>>,
    debt: AtomicUsize, // this debt count may only be updated by threads other than the current one - this has a `detached` flag as its last bit
    ref_cnt: AtomicUsize, // this ref cnt may only be updated by the current thread
}

impl<T: Send + Sync> Cache<T> {

    #[inline(always)]
    fn guard(&self) -> *const AtomicBool {
        unsafe { self.parent.byte_add(memoffset::offset_of!(InnerCachedArc<T>, dropping)).cast::<AtomicBool>() }
    }

    #[inline]
    fn get_debt_no_meta(&self, ordering: Ordering) -> usize {
        self.debt.load(ordering) & !DETACHED_FLAG
    }

}

fn cleanup_cache<T: Send + Sync>(cache: *const CachePadded<Cache<T>>, dropping_guard: *const AtomicBool, detached: bool) { // FIXME: when freeing the own memory, we still have a reference to it and thus UB
    // FIXME: the same thing is the case when freeing the major allocation
    // println!("cleanup cache: {}", unsafe { &*cache }.ref_cnt.load(Ordering::Relaxed));
    // FIXME: check for `detached` and deallocate own memory if detached
    // there are no external refs alive
    if unsafe { &*cache }.src.load(Ordering::Relaxed).is_null() {
        unsafe { &*cache }.debt.fetch_or(DETACHED_FLAG, Ordering::Relaxed);
        // we are the first "(cache) node", so we need to free the memory
        fence(Ordering::AcqRel); // FIXME: make sure this fence is coupled with some other atomic operation!
        unsafe { drop_slow::<T>(unsafe { &*cache }.parent.cast_mut()); }

        #[cold]
        unsafe fn drop_slow<T: Send + Sync>(ptr: *mut InnerCachedArc<T>) {
            drop(unsafe { SizedBox::from_ptr(NonNull::new_unchecked(ptr)) });
        }
    } else {
        // FIXME: we have other caches above us, so release a reference to them - this has to happen recursively tho as they could have to release their last reference as well.
        // FIXME: this should work - now make it iterative instead of recursive!
        if !unsafe { &*dropping_guard }.load(Ordering::Relaxed) {
            let super_cache_ptr = unsafe { &*cache }.src.load(Ordering::Relaxed);
            let super_cache = unsafe { &*super_cache_ptr }; // FIXME: this can cause a use-after-free
            let debt = super_cache.debt.fetch_add(1, Ordering::Acquire) + 1; // we add 1 at the end to make sure we use the post-update value
            // `ref_cnt` can't be updated after the cache was detached, so we can safely test for the `detached` flag
            let super_detached = debt & DETACHED_FLAG != 0;
            if strip_flags(debt) == super_cache.ref_cnt.load(Ordering::Relaxed) {
                drop(super_cache);
                cleanup_cache(super_cache_ptr, dropping_guard, super_detached);
            }
        }
    }
    if detached {
        fence(Ordering::AcqRel); // FIXME: make sure this fence is coupled with some other atomic operation!
        unsafe { SizedBox::from_ptr(NonNull::new_unchecked(cache.cast_mut())) };
    }
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
        if !is_detached(cache.debt.load(Ordering::Acquire)) {
            // println!("dropping detachable ptr!");
            // unsafe { &*self.0 }.detached.store(true, Ordering::Relaxed);
            let ref_cnt = cache.ref_cnt.load(Ordering::Relaxed);
            let debt = cache.debt.fetch_or(DETACHED_FLAG, Ordering::Acquire); // FIXME: this probably wants some different ordering
            // here no race condition can occur because we are the only ones who can update `ref_cnt` // FIXME: but can `debt`'s orderings still make data races possible?
            if debt == ref_cnt {
                let guard = cache.guard();
                let cache_ptr = self.0;
                drop(cache);
                cleanup_cache(cache_ptr, guard, true);
            }
        } else {
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

const DETACHED_FLAG: usize = 1 << (usize::BITS - 1);
const DELETION_FLAG: usize = 1 << (usize::BITS - 2);

#[inline(always)]
const fn strip_flags(debt: usize) -> usize {
    debt & !(DETACHED_FLAG | DELETION_FLAG)
}

#[inline(always)]
const fn is_detached(debt: usize) -> bool {
    debt & DETACHED_FLAG != 0
}

#[inline(always)]
const fn is_deleted(debt: usize) -> bool {
    debt & DELETION_FLAG != 0
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

    fn as_ref(&self) -> &T {
        // SAFETY: This is safe because we know that alloc_ptr can't be zero
        // and because we know that alloc_ptr has to point to a valid
        // instance of T in memory
        unsafe { &*self.alloc_ptr.as_ptr() }
    }

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
        Self {
            alloc_ptr: ptr,
        }
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

/*
struct Guarded<T> {
    cnt: AtomicUsize,
    guarded: *const T,
}

impl<T> Guarded<T> {

    #[inline]
    fn new(val: T) -> Self {
        let ptr = SizedBox::new(val).into_ptr().as_ptr();
        Self {
            cnt: AtomicUsize::new(1),
            guarded: ptr,
        }
    }

    fn try_acq() -> Option<&T> {

    }
    
}*/
