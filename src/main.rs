#![feature(strict_provenance_atomic_ptr)]
#![feature(strict_provenance)]
#![feature(core_intrinsics)]

mod swap_arc_intermediate;
mod swap_arc_tls;
mod swap_arc;
mod swap_arc_tls_less_fence;

use std::mem;
use std::sync::Arc;

fn main() {
    let arc = Arc::new(4);

}
/*
fn test_leak_arc(arc: &Arc<i32>) {

}

fn leak_arc<'a, T: 'a>(val: Arc<T>) -> &'a Arc<T> {
    let ptr = addr_of!(val);
    mem::forget(val);
    unsafe { ptr.as_ref() }.unwrap()
}
*/