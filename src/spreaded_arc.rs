use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::process::abort;
use std::{mem, ptr};
use std::intrinsics::{likely, unlikely};
use std::sync::atomic::{AtomicUsize, fence, Ordering};
use crossbeam_utils::CachePadded;
use thread_local::ThreadLocal;

pub struct SpreadedArc<T: Send + Sync> {
    inner: *const InnerSpreadedArc<T>,
    cache: *const Cache<T>,
}

// unsafe impl<T: Send> Send for SpreadedArc<T> {}
// unsafe impl<T: Sync> Sync for SpreadedArc<T> {}
unsafe impl<T: Send + Sync> Send for SpreadedArc<T> {}
unsafe impl<T: Send + Sync> Sync for SpreadedArc<T> {}

impl<T: Send + Sync> SpreadedArc<T> {

    pub fn new(val: T) -> Self {
        let tmp = Box::new(InnerSpreadedArc {
            val,
            global_cnt: /*CachePadded::new(*/AtomicUsize::new(1)/*)*/,
            cache: Default::default(),
        });
        let inner = Box::leak(tmp) as *const InnerSpreadedArc<T>;
        let tmp = unsafe { inner.as_ref().unwrap_unchecked() };
        let cache = tmp.cache.get_or(|| {
            Cache {
                parent: inner,
                cached_cnt: CachePadded::new(AtomicUsize::new(1)),
            }
        }) as *const _;
        Self {
            inner,
            cache,
        }
    }

    #[inline(always)]
    fn inner(&self) -> &InnerSpreadedArc<T> {
        unsafe { &*self.inner }
    }

}

const MAX_REFCOUNT: usize = (isize::MAX) as usize;

impl<T: Send + Sync> Clone for SpreadedArc<T> {
    fn clone(&self) -> Self {
        let inner = self.inner;
        let cache = unsafe { self.inner.as_ref().unwrap_unchecked() }.cache.get_or(|| {
            Cache {
                parent: inner,
                cached_cnt: CachePadded::new(AtomicUsize::new(0)),
            }
        });
        // FIXME: only do local counting if it's worth it!
        if unlikely(cache.cached_cnt.fetch_add(1, Ordering::Relaxed) == 0) {
            // fence(Ordering::Acquire);
            unsafe { self.cache.as_ref().unwrap_unchecked().parent.as_ref().unwrap_unchecked() }.global_cnt.fetch_add(1, Ordering::Relaxed);
        }

        Self {
            inner: self.inner,
            cache: cache as *const _,
        }
    }
}

impl<T: Send + Sync> Drop for SpreadedArc<T> {
    #[inline]
    fn drop(&mut self) {
        if likely(unsafe { self.cache.as_ref().unwrap_unchecked() }.cached_cnt.fetch_sub(1, Ordering::Relaxed) != 1) {
            return;
        }

        if likely(self.inner().global_cnt.fetch_sub(1, Ordering::Release) != 1) {
            return;
        }
        fence(Ordering::Acquire);
        unsafe { drop_slow::<T>(self.inner.cast_mut()); }

        #[cold]
        unsafe fn drop_slow<T: Send + Sync>(ptr: *mut InnerSpreadedArc<T>) {
            drop(unsafe { Box::from_raw(ptr) });
        }
    }
}

impl<T: Send + Sync> AsRef<T> for SpreadedArc<T> {
    #[inline(always)]
    fn as_ref(&self) -> &T {
        unsafe { &*self.inner.cast::<T>() }
    }
}

impl<T: Send + Sync> Deref for SpreadedArc<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inner.cast::<T>() }
    }
}

#[repr(C)]
struct InnerSpreadedArc<T: Send + Sync> {
    val: T,
    global_cnt: /*CachePadded<*/AtomicUsize/*>*/,
    cache: ThreadLocal<Cache<T>>,
}

struct Cache<T: Send + Sync> {
    parent: *const InnerSpreadedArc<T>,
    cached_cnt: CachePadded<AtomicUsize>,
}

unsafe impl<T: Send + Sync> Send for Cache<T> {}
unsafe impl<T: Send + Sync> Sync for Cache<T> {}
