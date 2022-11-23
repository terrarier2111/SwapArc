use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::process::abort;
use std::{mem, ptr, thread};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, fence, Ordering};
use crossbeam_utils::CachePadded;
use thread_local::ThreadLocal;

pub struct AutoLocalArc<T: Send + Sync> {
    inner: *const InnerCachedArc<T>,
    cache: UnsafeCell<*const CachePadded<Cache<T>>>,
}

// unsafe impl<T: Send> Send for CachedArc<T> {}
// unsafe impl<T: Sync> Sync for CachedArc<T> {}
unsafe impl<T: Send + Sync> Send for AutoLocalArc<T> {}
unsafe impl<T: Send + Sync> Sync for AutoLocalArc<T> {}

impl<T: Send + Sync> AutoLocalArc<T> {

    pub fn new(val: T) -> Self {
        let tmp = Box::new(InnerCachedArc {
            val,
            cache: Default::default(),
        });
        let inner = Box::leak(tmp) as *mut InnerCachedArc<T>;
        let cache = unsafe { &*inner }.cache.get_or(|| {
            DetachablePtr(Box::leak(Box::new(CachePadded::new(Cache {
                parent: inner,
                ref_cnt: AtomicUsize::new(1),
                debt: Default::default(),
                thread_id: thread_id(),
                src: AtomicPtr::new(null_mut()), // we have no src, as we are the src ourselves
            }))))
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
        let cache = unsafe { &**self.cache.get() };
        let cache = if cache.thread_id == thread_id() {
            let ref_cnt = cache.ref_cnt.load(Ordering::SeqCst);
            if ref_cnt > MAX_REFCOUNT {
                unsafe { handle_large_ref_count(cache, ref_cnt); }
            }
            cache.ref_cnt.store(ref_cnt + 1, Ordering::SeqCst);
            unsafe { *self.cache.get() }
        } else {
            let inner = self.inner;
            let local_cache = unsafe { &*self.inner().cache.get_or(|| {
                DetachablePtr(Box::leak(Box::new(CachePadded::new(Cache {
                    parent: inner,
                    src: AtomicPtr::new((cache as *const CachePadded<Cache<T>>).cast_mut()),
                    debt: Default::default(),
                    thread_id: thread_id(),
                    ref_cnt: AtomicUsize::new(0),
                }))))
            }).0 };
            if local_cache.ref_cnt.load(Ordering::SeqCst) == local_cache.debt.load(Ordering::SeqCst) {
                // the local cache has no valid references anymore
                local_cache.src.store((cache as *const CachePadded<Cache<T>>).cast_mut(), Ordering::SeqCst); // FIXME: can the lack of synchronization between these three updates lead to race conditions?
                local_cache.debt.store(0, Ordering::SeqCst);
                local_cache.ref_cnt.store(1, Ordering::SeqCst);
                // remove the reference to the external cache, as we moved it to our own cache instead
                *unsafe { &mut *self.cache.get() } = local_cache as *const _;
            } else {
                // the local cache is still in use
                let ref_cnt = local_cache.ref_cnt.load(Ordering::SeqCst);
                if ref_cnt > MAX_REFCOUNT {
                    unsafe { handle_large_ref_count(local_cache, ref_cnt); }
                }
                local_cache.ref_cnt.store(ref_cnt + 1, Ordering::SeqCst);
                // FIXME: here is probably a possibility for a race condition (what if we have one of the instances on another thread and it gets freed right
                // FIXME: after we check if ref_cnt == debt?) this can probably be fixed by checking again right after increasing the ref_count and
                // FIXME: handling failure by doing what we would have done if the local cache had no valid references to begin with.
            }

            local_cache as *const _
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
    let debt = cache.debt.swap(0, Ordering::SeqCst); // FIXME: can the lack of synchronization between these two updates lead to race conditions?
    let ref_cnt = ref_cnt - debt;
    cache.ref_cnt.store(ref_cnt, Ordering::SeqCst);
    if ref_cnt > MAX_HARD_REFCOUNT {
        abort();
    }
}

impl<T: Send + Sync> Drop for AutoLocalArc<T> {
    #[inline]
    fn drop(&mut self) {
        let cache = unsafe { &**self.cache.get() };
        if cache.thread_id == thread_id() {
            let ref_cnt = cache.ref_cnt.load(Ordering::SeqCst);
            println!("dropping {}", ref_cnt);
            cache.ref_cnt.store(ref_cnt - 1, Ordering::SeqCst);
            let ref_cnt = ref_cnt - 1;
            // `ref_cnt` can't race with `debt` here because we are the only ones who are able to update `ref_cnt`
            let debt = cache.debt.load(Ordering::SeqCst);
            println!("debt: {}\nref_cnt: {}", strip_flags(debt), ref_cnt);
            if ref_cnt == strip_flags(debt) {
                // there are no external refs alive
                cache.cleanup(is_detached(debt));
            }
        } else {
            let new_refs = strip_flags(cache.debt.fetch_add(1, Ordering::SeqCst)) + 1;
            if new_refs > MAX_HARD_REFCOUNT {
                abort();
            }
            if new_refs == cache.ref_cnt.load(Ordering::SeqCst) { // FIXME: ref_cnt can probably race with `debt` here
                                                                         // FIXME: so what we need to do is add further validation after this check
                                                                         // FIXME: this is probably in form of HazardPointers or the like
                // there are no external refs alive
                cache.cleanup(is_detached(new_refs));
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
}

struct Cache<T: Send + Sync> {
    parent: *const InnerCachedArc<T>,
    thread_id: u64,
    src: AtomicPtr<CachePadded<Cache<T>>>,
    debt: AtomicUsize, // this debt count may only be updated by threads other than the current one - this has a `detached` flag as its last bit
    ref_cnt: AtomicUsize, // this ref cnt may only be updated by the current thread
}

impl<T: Send + Sync> Cache<T> {

    fn cleanup(self: &CachePadded<Self>, detached: bool) {
        println!("cleanup cache!");
        // FIXME: check for `detached` and deallocate own memory if detached
        // there are no external refs alive
        if self.src.load(Ordering::SeqCst).is_null() {
            // we are the first "(cache) node", so we need to free the memory
            fence(Ordering::AcqRel); // FIXME: make sure this fence is coupled with some other atomic operation!
            unsafe { drop_slow::<T>(self.parent.cast_mut()); }

            #[cold]
            unsafe fn drop_slow<T: Send + Sync>(ptr: *mut InnerCachedArc<T>) {
                drop(unsafe { Box::from_raw(ptr) });
            }
        } else {
            // FIXME: we have other caches above us, so release a reference to them - this has to happen recursively tho as they could have to release their last reference as well.
            // FIXME: this should work - now make it iterative instead of recursive!
            let super_cache = unsafe { &*self.src.load(Ordering::SeqCst) };
            let debt = super_cache.debt.fetch_add(1, Ordering::SeqCst) + 1; // we add 1 at the end to make sure we use the post-update value
            // `ref_cnt` can't be updated after the cache was detached, so we can safely test for the `detached` flag
            let super_detached = debt & DETACHED_FLAG != 0;
            if strip_flags(debt) == super_cache.ref_cnt.load(Ordering::SeqCst) {
                super_cache.cleanup(super_detached);
            }
        }
        if detached {
            fence(Ordering::AcqRel); // FIXME: make sure this fence is coupled with some other atomic operation!
            unsafe { Box::from_raw((self as *const CachePadded<Self>).cast_mut()) };
        }
    }

    #[inline]
    fn get_debt_no_meta(&self, ordering: Ordering) -> usize {
        self.debt.load(ordering) & !DETACHED_FLAG
    }

}

// unsafe impl<T: Send> Send for Cache<T> {}
// unsafe impl<T: Sync> Sync for Cache<T> {}
unsafe impl<T: Send + Sync> Send for Cache<T> {}
unsafe impl<T: Send + Sync> Sync for Cache<T> {}

#[derive(Clone)]
#[repr(transparent)]
struct DetachablePtr<T: Send + Sync>(*const CachePadded<Cache<T>>);

/*
impl<T: Send + Sync> DetachablePtr<T> {

    #[inline]
    fn detach(self) {
        unsafe { &*self.0 }.detached.store(true, Ordering::SeqCst);
    }

}*/

impl<T: Send + Sync> Drop for DetachablePtr<T> {
    #[inline]
    fn drop(&mut self) {
        // FIXME: only detach if there are external references left! - this could be a bit tricky but we can probably use HazardPointers or reference counter
        // FIXME: tagging if there's no other good option!
        if !self.0.is_null() {
            println!("dropping detachable ptr!");
            // unsafe { &*self.0 }.detached.store(true, Ordering::SeqCst);
            let cache = unsafe { &*self.0 };
            let ref_cnt = cache.ref_cnt.load(Ordering::SeqCst);
            let debt = cache.debt.fetch_or(DETACHED_FLAG, Ordering::SeqCst); // FIXME: this probably wants some different ordering
            // here no race condition can occur because we are the only ones who can update `ref_cnt` // FIXME: but can `debt`'s orderings still make data races possible?
            if debt == ref_cnt {
                cache.cleanup(true);
            }
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

#[inline(always)]
const fn strip_flags(debt: usize) -> usize {
    debt & !DETACHED_FLAG
}

#[inline(always)]
const fn is_detached(debt: usize) -> bool {
    debt & DETACHED_FLAG != 0
}
