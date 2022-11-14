#![feature(core_intrinsics)]

use std::sync::Arc;
pub use swap_arc_tls_optimistic::{RefCnt, DataPtrConvert };
use crate::swap_arc_tls_optimistic::SwapArcIntermediateTLS;

mod swap_arc_tls_optimistic;

pub type SwapArc<T> = SwapArcIntermediateTLS<T, Arc<T>, 0>;
pub type SwapArcOption<T> = SwapArcIntermediateTLS<T, Option<Arc<T>>, 0>;
pub type SwapArcAny<T, D> = SwapArcIntermediateTLS<T, D, 0>;
pub type SwapArcAnyMeta<T, D, const METADATA_BITS: u32> = SwapArcIntermediateTLS<T, D, METADATA_BITS>;

#[test]
fn test_load() {
    let instance = SwapArc::new(Arc::new("test!!!"));

}
