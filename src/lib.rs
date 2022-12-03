#![feature(core_intrinsics)]

use std::sync::Arc;

use crate::swap_arc_tls_optimistic::SwapArcIntermediateTLS;
pub use swap_arc_tls_optimistic::{DataPtrConvert, RefCnt};

mod swap_arc_tls_optimistic;

pub type SwapArc<T> = SwapArcIntermediateTLS<T, Arc<T>, 0>;
pub type SwapArcOption<T> = SwapArcIntermediateTLS<T, Option<Arc<T>>, 0>;
pub type SwapArcAny<T, D> = SwapArcIntermediateTLS<T, D, 0>;
pub type SwapArcAnyMeta<T, D, const METADATA_BITS: u32> =
    SwapArcIntermediateTLS<T, D, METADATA_BITS>;

#[cfg(all(test, not(miri)))]
#[test]
fn test_load_multi() {
    use std::hint::black_box;
    use std::thread;
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> =
        Arc::new(SwapArcIntermediateTLS::new(Arc::new(3)));
    let mut threads = vec![];
    for _ in 0..20 {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..2000 {
                let l1 = tmp.load();
                black_box(l1);
            }
        }));
    }
    for _ in 0..20 {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..2000 {
                tmp.update(Arc::new(rand::random()));
            }
        }));
    }
    threads
        .into_iter()
        .for_each(|thread| thread.join().unwrap());
}

#[test]
fn test_load() {
    let tmp = SwapArc::new(Arc::new(3));
    tmp.load();
}

#[test]
fn test_store() {
    let tmp = SwapArc::new(Arc::new(3));
    tmp.update(Arc::new(-2));
}
