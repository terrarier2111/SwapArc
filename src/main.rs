#![feature(strict_provenance_atomic_ptr)]
#![feature(strict_provenance)]
#![feature(core_intrinsics)]
#![feature(const_option_ext)]

mod swap_arc_intermediate;
mod swap_arc_tls;
mod swap_arc;
mod swap_arc_tls_less_fence;
mod swap_arc_tls_optimistic;

use std::{mem, thread};
use std::sync::Arc;
use crate::swap_arc_tls_optimistic::SwapArcIntermediateTLS;

fn main() {
    let arc = Arc::new(4);
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> = SwapArcIntermediateTLS::new(arc);

    let mut threads = vec![];
    tmp.update(Arc::new(31));
    println!("{}", tmp.load());
    for _ in 0..20/*5*//*1*/ {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for x in 0..10/*200*/ {
                println!("{}", tmp.load());
                if x % 5 == 0 {
                    println!("completed push: {x}");
                }
                // thread::sleep(Duration::from_millis(1000));
            }
        }));
    }
    for _ in 0..20/*5*//*1*/ {
        // let send = send.clone();
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            // let send = send.clone();
            for x in 0..50/*200*/ {
                /*
                thread::sleep(Duration::from_millis(500));
                println!("{:?}", list.remove_head());
                thread::sleep(Duration::from_millis(500));*/
                // send.send(list.remove_head()).unwrap();
                tmp.update(Arc::new(rand::random()));

                if x % 5 == 0 {
                    println!("completed removals: {x}");
                }
            }
        }));
    }
    threads.into_iter().for_each(|thread| thread.join().unwrap());

    // loop {}
    println!("{:?}", tmp.load().as_ref());
    tmp.update(Arc::new(6));
    println!("{:?}", tmp.load().as_ref());
    println!("test! 0");
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