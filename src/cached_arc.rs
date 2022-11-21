use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::process::abort;
use std::{mem, ptr};
use std::sync::atomic::{AtomicUsize, fence, Ordering};
use crossbeam_utils::CachePadded;
use thread_local::ThreadLocal;

pub struct CachedArc<T: Send + Sync> {
    inner: *const InnerCachedArc<T>,
}

// unsafe impl<T: Send> Send for CachedArc<T> {}
// unsafe impl<T: Sync> Sync for CachedArc<T> {}
unsafe impl<T: Send + Sync> Send for CachedArc<T> {}
unsafe impl<T: Send + Sync> Sync for CachedArc<T> {}

impl<T: Send + Sync> CachedArc<T> {

    pub fn new(val: T) -> Self {
        let tmp = Box::new(InnerCachedArc {
            val,
            global_cnt: /*CachePadded::new(*/AtomicUsize::new(1)/*)*/,
            cache: Default::default(),
        });
        Self {
            inner: Box::leak(tmp) as *const InnerCachedArc<T>,
        }
    }

    pub fn downgrade(self) -> CachedLocalArc<T> {
        let curr = self.inner;
        let curr = unsafe { &*self.inner }.cache.get_or(|| {
            CachePadded::new(Cache {
                parent: curr,
                cached_cnt: UnsafeCell::new(CachedCount { ref_cnt: 0, debt: 0 }),
            })
        });

        let cnts = unsafe { &mut *curr.cached_cnt.get() };
        let inner = self.inner;

        cnts.ref_cnt += 1;
        if cnts.ref_cnt == 1 {
            cnts.debt += 1;
            mem::forget(self);
        }

        CachedLocalArc {
            inner,
            cache: curr,
        }
    }

    pub fn downgrade_cloned(&self) -> CachedLocalArc<T> {
        let curr = self.inner;
        let curr = self.inner().cache.get_or(|| {
            CachePadded::new(Cache {
                parent: curr,
                cached_cnt: UnsafeCell::new(CachedCount { ref_cnt: 0, debt: 0 }),
            })
        });

        let cnts = unsafe { &mut *curr.cached_cnt.get() };

        cnts.ref_cnt += 1;
        if cnts.ref_cnt == 1 {
            cnts.debt += 1;
            // increment the strong count
            mem::forget(self.clone());
        }

        CachedLocalArc {
            inner: self.inner,
            cache: curr as *const _,
        }
    }

    #[inline(always)]
    fn inner(&self) -> &InnerCachedArc<T> {
        unsafe { &*self.inner }
    }

}

const MAX_REFCOUNT: usize = (isize::MAX) as usize;

impl<T: Send + Sync> Clone for CachedArc<T> {
    #[inline]
    fn clone(&self) -> Self {
        let old_size = self.inner().global_cnt.fetch_add(1, Ordering::Relaxed);

        if old_size > MAX_REFCOUNT {
            abort();
        }

        Self {
            inner: self.inner,
        }
    }
}

impl<T: Send + Sync> Drop for CachedArc<T> {
    #[inline]
    fn drop(&mut self) {
        if let Some(curr) = self.inner().cache.get() {
            let cnts = unsafe { &mut *curr.cached_cnt.get() };
            if cnts.ref_cnt != 0 {
                // just store the debt
                cnts.debt += 1;
                return;
            }
        }

        if self.inner().global_cnt.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);
        unsafe { drop_slow::<T>(self.inner.cast_mut()); }

        #[cold]
        unsafe fn drop_slow<T: Send + Sync>(ptr: *mut InnerCachedArc<T>) {
           drop(unsafe { Box::from_raw(ptr) });
        }
    }
}

impl<T: Send + Sync> AsRef<T> for CachedArc<T> {
    #[inline(always)]
    fn as_ref(&self) -> &T {
        unsafe { &*self.inner.cast::<T>() }
    }
}

impl<T: Send + Sync> Deref for CachedArc<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inner.cast::<T>() }
    }
}

pub struct CachedLocalArc<T: Send + Sync> {
    inner: *const InnerCachedArc<T>,
    cache: *const CachePadded<Cache<T>>,
}

impl<T: Send + Sync> CachedLocalArc<T> {

    pub fn upgrade(self) -> CachedArc<T> {
        let cache = unsafe { &*self.cache };
        let cnt = unsafe { &mut *cache.cached_cnt.get() };
        cnt.ref_cnt -= 1;
        if cnt.debt > 1 {
            cnt.debt -= 1;
            let inner = self.inner;
            mem::forget(self);
            return CachedArc {
                inner,
            };
        }

        let old_size = self.inner().global_cnt.fetch_add(1, Ordering::Relaxed);

        if old_size > MAX_REFCOUNT {
            abort();
        }
        
        CachedArc {
            inner: self.inner,
        }
    }

    #[inline(always)]
    fn inner(&self) -> &InnerCachedArc<T> {
        unsafe { &*self.inner }
    }

}

impl<T: Send + Sync> Clone for CachedLocalArc<T> {
    #[inline]
    fn clone(&self) -> Self {
        let curr = unsafe { self.inner().cache.get().unwrap_unchecked() };
        let cnts = unsafe { &mut *curr.cached_cnt.get() };
        cnts.ref_cnt += 1;

        Self {
            inner: self.inner,
            cache: curr as *const _,
        }
    }
}

impl<T: Send + Sync> Drop for CachedLocalArc<T> {
    #[inline]
    fn drop(&mut self) {
        let curr = unsafe { self.inner().cache.get().unwrap_unchecked() };
        let cnts = unsafe { &mut *curr.cached_cnt.get() };
        cnts.ref_cnt -= 1;
        if cnts.ref_cnt == 0 {
            let debt = cnts.debt;
            cnts.debt = 0;
            if self.inner().global_cnt.fetch_sub(debt, Ordering::Release) != debt {
                return;
            }
            fence(Ordering::Acquire);
            unsafe { drop_slow::<T>(self.inner.cast_mut()); }
        }

        #[cold]
        unsafe fn drop_slow<T: Send + Sync>(ptr: *mut InnerCachedArc<T>) {
            drop(unsafe { Box::from_raw(ptr) });
        }
    }
}

impl<T: Send + Sync> AsRef<T> for CachedLocalArc<T> {
    #[inline(always)]
    fn as_ref(&self) -> &T {
        unsafe { &*self.inner.cast::<T>() }
    }
}

impl<T: Send + Sync> Deref for CachedLocalArc<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inner.cast::<T>() }
    }
}

#[repr(C)]
struct InnerCachedArc<T: Send + Sync> {
    val: T,
    global_cnt: /*CachePadded<*/AtomicUsize/*>*/,
    cache: ThreadLocal<CachePadded<Cache<T>>>,
}

struct Cache<T: Send + Sync> {
    parent: *const InnerCachedArc<T>,
    cached_cnt: UnsafeCell<CachedCount>,
}

// unsafe impl<T: Send> Send for Cache<T> {}
// unsafe impl<T: Sync> Sync for Cache<T> {}
unsafe impl<T: Send + Sync> Send for Cache<T> {}
unsafe impl<T: Send + Sync> Sync for Cache<T> {}

struct CachedCount {
    ref_cnt: usize,
    debt: usize,
}
