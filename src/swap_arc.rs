
// FIXME: add ability to load non-fully
/*
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
pub struct SwapArc<T> {
    ref_cnt: AtomicUsize, // the last bit is the `update` bit
    ptr: AtomicPtr<T>,
    updated: AtomicPtr<T>,
}

impl<T> SwapArc<T> {

    const UPDATE: usize = 1 << (usize::BITS - 1);
    const FORCE_UPDATE: usize = 1 << (usize::BITS - 1); // FIXME: do we actually need a separate flag? - we probably do
    // FIXME: implement force updating!
    pub fn load_full(&self) -> Arc<T> {
        let mut ref_cnt = self.ref_cnt.fetch_add(1, Ordering::SeqCst);
        // wait until the update finished
        while ref_cnt & Self::UPDATE != 0 {
            ref_cnt = self.ref_cnt.load(Ordering::SeqCst);
        }
        let tmp = unsafe { Arc::from_raw(self.ptr.load(Ordering::Acquire)) };
        let ret = tmp.clone();
        // create a `virtual reference` to the Arc to ensure it doesn't get dropped until we can allow it to be so
        mem::forget(tmp);
        let curr = self.ref_cnt.fetch_sub(1, Ordering::SeqCst);
        // fast-rejection path to ensure we are only trying to update if it's worth it
        if (curr == 0/* || curr == Self::UPDATE*/) && !self.updated.load(Ordering::SeqCst).is_null() {
            self.try_update();
        }
        ret
    }

    fn try_update(&self) {
        match self.ref_cnt.compare_exchange(0, Self::UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_)/* | Err(Self::UPDATE)*/ => {
                // take the update
                let update = self.updated.swap(null_mut(), Ordering::SeqCst);
                // check if we even have an update
                if !update.is_null() {
                    // update the pointer
                    let prev = self.ptr.swap(update, Ordering::Release);
                    // unset the update flag
                    self.ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                    // drop the `virtual reference` we hold to the Arc
                    unsafe { Arc::from_raw(prev); }
                } else {
                    // unset the update flag
                    self.ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                }
            }
            _ => {}
        }
    }

    pub fn update(&self, updated: Arc<T>) {
        loop {
            match self.ref_cnt.compare_exchange(0, Self::UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    // clear out old updates to make sure our update won't be overwritten by them in the future
                    self.updated.store(null_mut(), Ordering::SeqCst);
                    let prev = self.ptr.swap(Arc::into_raw(updated).cast_mut(), Ordering::Release);
                    // unset the update flag
                    self.ref_cnt.fetch_and(!Self::UPDATE, Ordering::SeqCst);
                    // drop the `virtual reference` we hold to the Arc
                    unsafe { Arc::from_raw(prev); }
                    break;
                }
                Err(old) => {
                    if old & Self::UPDATE != 0 {
                        // somebody else already updates the current ptr, so we wait until they finish their update
                        continue;
                    }
                    // push our update up, so it will be applied in the future
                    self.updated.store(Arc::into_raw(updated).cast_mut(), Ordering::SeqCst);
                    break;
                }
            }
        }
    }

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

#[derive(Copy, Clone, Debug)]
pub enum UpdateResult {
    Ok,
    AlreadyUpdating,
    NoUpdate,
}*/