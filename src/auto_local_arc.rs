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
        let inner = Box::leak(tmp) as *const InnerCachedArc<T>;
        let cache = unsafe { &*inner }.cache.get_or(|| {
            DetachablePtr(Box::leak(Box::new(CachePadded::new(Cache {
                parent: inner,
                ref_cnt: AtomicUsize::new(1),
                debt: Default::default(),
                thread_id: thread_id(),
                src: AtomicPtr::new(null_mut()), // we have no src, as we are the src ourselves
                detached: AtomicBool::new(false),
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

const MAX_REFCOUNT: usize = (isize::MAX) as usize / 2;
const MAX_HARD_REFCOUNT: usize = (isize::MAX) as usize;

impl<T: Send + Sync> Clone for AutoLocalArc<T> {
    fn clone(&self) -> Self {
        let cache = unsafe { &**self.cache.get() };
        let cache = if cache.thread_id == thread_id() {
            let ref_cnt = cache.ref_cnt.load(Ordering::Relaxed);
            if ref_cnt > MAX_REFCOUNT {
                #[cold]
                fn handle_large_ref_count<T: Send + Sync>(cache: &CachePadded<Cache<T>>, ref_cnt: usize) {
                    // try to recover by decrementing the `debt` count from the `ref_cnt` and setting the `debt` count to 0
                    let debt = cache.debt.swap(0, Ordering::Relaxed);
                    let ref_cnt = ref_cnt - debt;
                    cache.ref_cnt.store(ref_cnt, Ordering::Relaxed);
                    if ref_cnt > MAX_HARD_REFCOUNT {
                        abort();
                    }
                }
                handle_large_ref_count(cache, ref_cnt);
            }
            cache.ref_cnt.store(ref_cnt + 1, Ordering::Relaxed);
            unsafe { *self.cache.get() }
        } else {
            let inner = self.inner;
            let local_cache = unsafe { &*self.inner().cache.get_or(|| {
                DetachablePtr(Box::leak(Box::new(CachePadded::new(Cache {
                    parent: inner,
                    src: AtomicPtr::new((cache as *const CachePadded<Cache<T>>).cast_mut()),
                    debt: Default::default(),
                    thread_id: thread_id(),
                    ref_cnt: AtomicUsize::new(1),
                    detached: AtomicBool::new(false),
                }))))
            }).0 };
            local_cache.src.store((cache as *const CachePadded<Cache<T>>).cast_mut(), Ordering::Relaxed);
            // remove the reference to the external cache, as we moved it to our own cache instead
            *unsafe { &mut *self.cache.get() } = local_cache as *const _;
            local_cache as *const _
        };

        Self {
            inner: self.inner,
            cache: cache.into(),
        }
    }
}

impl<T: Send + Sync> Drop for AutoLocalArc<T> {
    #[inline]
    fn drop(&mut self) {
        let cache = unsafe { &**self.cache.get() };
        if cache.thread_id == thread_id() {
            let ref_cnt = cache.ref_cnt.load(Ordering::Relaxed);
            cache.ref_cnt.store(ref_cnt - 1, Ordering::Relaxed);
            if ref_cnt == cache.debt.load(Ordering::Relaxed) {
                // there are no external refs alive
                cache.cleanup();
            }
        } else {
            let new_refs = cache.debt.fetch_add(1, Ordering::Relaxed);
            if new_refs > MAX_HARD_REFCOUNT {
                abort();
            }
            if new_refs == cache.ref_cnt.load(Ordering::Relaxed) {
                // there are no external refs alive
                cache.cleanup();
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
    cache: ThreadLocal<DetachablePtr<T>>, // FIXME: only store a pointer to a `CachePadded<Cache<T>>` in here.
}

struct Cache<T: Send + Sync> {
    parent: *const InnerCachedArc<T>,
    thread_id: u64,
    src: AtomicPtr<CachePadded<Cache<T>>>,
    debt: AtomicUsize, // this debt count may only be updated by threads other than the current one.
    ref_cnt: AtomicUsize, // this ref cnt may only be updated by the current thread
    detached: AtomicBool,
}

impl<T: Send + Sync> Cache<T> {

    fn cleanup(&self) {
        // there are no external refs alive
        if self.src.load(Ordering::Relaxed).is_null() {
            // we are the first "(cache) node", so we need to free the memory
            fence(Ordering::AcqRel);
            unsafe { drop_slow::<T>(self.parent.cast_mut()); }

            #[cold]
            unsafe fn drop_slow<T: Send + Sync>(ptr: *mut InnerCachedArc<T>) {
                drop(unsafe { Box::from_raw(ptr) });
            }
        } else {
            // FIXME: we have other caches above us, so release a reference to them - this has to happen recursively tho as they could have to release their last reference as well.
            // FIXME: this should work - now make it iterative instead of recursive!
            let cache = unsafe { &*self.src.load(Ordering::Relaxed) };
            let ref_cnt = cache.debt.fetch_add(1, Ordering::Relaxed);
            if ref_cnt == cache.ref_cnt.load(Ordering::Relaxed) {
                cache.cleanup();
            }
        }
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
        unsafe { &*self.0 }.detached.store(true, Ordering::Relaxed);
    }

}*/

impl<T: Send + Sync> Drop for DetachablePtr<T> {
    #[inline]
    fn drop(&mut self) {
        unsafe { &*self.0 }.detached.store(true, Ordering::Relaxed);
    }
}

unsafe impl<T: Send + Sync> Send for DetachablePtr<T> {}

#[inline]
fn thread_id() -> u64 {
    // thread::current().id().as_u64().get()
    thread_id::get() as u64
}
