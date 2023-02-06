#![feature(core_intrinsics)]
#![feature(arbitrary_self_types)]
// only for testing!
#![allow(soft_unstable)]
#![feature(test)]
#![feature(bench_black_box)]
#![feature(thread_id_value)]
#![feature(pointer_byte_offsets)]

mod swap_arc_tls_optimistic;

use crate::swap_arc_tls_optimistic::SwapArcIntermediateTLS;
use std::hint::{black_box, spin_loop};
use std::mem::transmute;
use std::ops::Range;
use loom::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{mem, ptr};
use loom::thread;

#[cfg(test)]
extern crate test;
#[cfg(test)]
use arc_swap::ArcSwap;
#[cfg(test)]
use test::Bencher;

fn main() {
    loom::model(|| {
        let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> =
            Arc::new(SwapArcIntermediateTLS::new(Arc::new(0)));
        for _ in 0..10000 {
            let mut threads = vec![];
            for _ in 0..1 {
                let tmp = tmp.clone();
                threads.push(thread::spawn(move || {
                    loom::model(move || {
                        for x in 0..2000 {
                            // FIXME: the abort only happens when there are two values loaded at the same time (l1 and l2 in this case)
                            // FIXME: with just l1 nothing happens!

                            // FIXME: the "unaligned fastbin chunk..." happens in the writer/updater thread (at least sometimes)
                            // FIXME: from that we can deduce that `curr` (the thing its ptr points to) has to have an invalid
                            // FIXME: reference count because everything else either doesn't get touched by `store` or it just gets `swapped`
                            // FIXME: by the reader and thus can't be present and reachable by the updater and have an invalid reference count at the same time!
                            // FIXME: Furthermore there have to be 2 subsequent stores in order for the abort to happen!
                            let l1 = tmp.load();
                            let l2 = tmp.load();
                            /*let l3 = tmp.load();
                    let l4 = tmp.load();
                    let l5 = tmp.load();*/
                            /*if l1.gen_cnt != l2.gen_cnt {
                        println!("diff!");
                    }*/
                            black_box(l1);
                            black_box(l2);
                            if (x + 1) % 1000 == 0 || x == 0 {
                                println!("read {}", x);
                            }
                            /*black_box(l3);
                    black_box(l4);
                    black_box(l5);*/
                        }
                    });
                }));
            }
            for _ in 0..1
            /*5*//*1*/
            {
                // let send = send.clone();
                let tmp = tmp.clone();
                threads.push(thread::spawn(move || {
                    loom::model(move || {
                        // let send = send.clone();
                        for x in 0..2
                        /*200*/
                        {
                            tmp.store(Arc::new(rand::random()));
                            /*if x % 1000 == 0 {
                        println!("write {}", x);
                    }*/
                        }
                    });
                }));
            }
            threads
                .into_iter()
                .for_each(|thread| thread.join().unwrap());
        }
    });
}

fn bad_bench_us_multi() {
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> =
        Arc::new(SwapArcIntermediateTLS::new(Arc::new(0)));
    let mut many_threads = Arc::new(Mutex::new(vec![]));
    for _ in 0..10 {
        let started = Arc::new(AtomicBool::new(false));
        let mut threads = vec![];
        for _ in 0..20
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            let started = started.clone();
            threads.push(thread::spawn(move || {
                while !started.load(Ordering::Acquire) {
                    spin_loop();
                }

                for _ in 0..2000
                /*200*/
                {
                    let l1 = tmp.load();
                    let l2 = tmp.load();
                    let l3 = tmp.load();
                    let l4 = tmp.load();
                    let l5 = tmp.load();
                    black_box(l1);
                    black_box(l2);
                    black_box(l3);
                    black_box(l4);
                    black_box(l5);
                }
            }));
        }
        for _ in 0..20 {
            let tmp = tmp.clone();
            let started = started.clone();
            threads.push(thread::spawn(move || {
                while !started.load(Ordering::Acquire) {
                    spin_loop();
                }

                for _ in 0..2000 {
                    tmp.store(Arc::new(rand::random()));
                }
            }));
        }
        many_threads.lock().unwrap().push((threads, started));
    }
    for _ in 0..10 {
        let start = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let (threads, started) = many_threads.clone().lock().unwrap().remove(0);
        started.store(true, Ordering::Release);
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        println!("test took: {}ms", time - start);
    }
}

/*
#[bench]
fn bench_us_multi(bencher: &mut Bencher) {
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> = SwapArcIntermediateTLS::new(Arc::new(0));
    let mut many_threads = Arc::new(Mutex::new(vec![]));
    for _ in 0..100 {
        let started = Arc::new(AtomicBool::new(false));
        let mut threads = vec![];
        for _ in 0..5/*5*//*1*/ {
            let tmp = tmp.clone();
            let started = started.clone();
            threads.push(thread::spawn(move || {
                while !started.load(Ordering::Acquire) {
                    spin_loop();
                }
                for _ in 0..2000/*200*/ {
                    let l1 = tmp.load();
                    let l2 = tmp.load();
                    let l3 = tmp.load();
                    let l4 = tmp.load();
                    let l5 = tmp.load();
                    black_box(l1);
                    black_box(l2);
                    black_box(l3);
                    black_box(l4);
                    black_box(l5);
                }
            }));
        }
        for _ in 0..5/*5*//*1*/ {
            // let send = send.clone();
            let tmp = tmp.clone();
            let started = started.clone();
            threads.push(thread::spawn(move || {
                while !started.load(Ordering::Acquire) {
                    spin_loop();
                }
                for _ in 0..2000/*200*/ {
                    tmp.update(Arc::new(rand::random()));
                }
            }));
        }
        many_threads.lock().unwrap().push((threads, started));
    }
    bencher.iter(|| {
        let (threads, started) = many_threads.clone().lock().unwrap().remove(0);
        started.store(true, Ordering::Release);
        threads.into_iter().for_each(|thread| thread.join().unwrap());
    });
}
#[bench]
fn bench_other_multi(bencher: &mut Bencher) {
    let tmp = Arc::new(ArcSwap::new(Arc::new(0)));
    let mut many_threads = Arc::new(Mutex::new(vec![]));
    for _ in 0..100 {
        let started = Arc::new(AtomicBool::new(false));
        let mut threads = vec![];
        for _ in 0..5/*5*//*1*/ {
            let tmp = tmp.clone();
            let started = started.clone();
            threads.push(thread::spawn(move || {
                while !started.load(Ordering::Acquire) {
                    spin_loop();
                }
                for _ in 0..2000/*200*/ {
                    let l1 = tmp.load();
                    let l2 = tmp.load();
                    let l3 = tmp.load();
                    let l4 = tmp.load();
                    let l5 = tmp.load();
                    black_box(l1);
                    black_box(l2);
                    black_box(l3);
                    black_box(l4);
                    black_box(l5);
                }
            }));
        }
        for _ in 0..5/*5*//*1*/ {
            // let send = send.clone();
            let tmp = tmp.clone();
            let started = started.clone();
            threads.push(thread::spawn(move || {
                while !started.load(Ordering::Acquire) {
                    spin_loop();
                }
                for _ in 0..2000/*200*/ {
                    tmp.store(Arc::new(rand::random()));
                }
            }));
        }
        many_threads.lock().unwrap().push((threads, started));
    }
    bencher.iter(|| {
        let (threads, started) = many_threads.clone().lock().unwrap().remove(0);
        started.store(true, Ordering::Release);
        threads.into_iter().for_each(|thread| thread.join().unwrap());
    });
}*/

#[bench]
fn bench_us_multi(bencher: &mut Bencher) {
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> =
        Arc::new(SwapArcIntermediateTLS::new(Arc::new(0)));
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20 {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..20000 {
                    let l1 = tmp.load();
                    let l2 = tmp.load();
                    let l3 = tmp.load();
                    let l4 = tmp.load();
                    let l5 = tmp.load();
                    black_box(l1);
                    black_box(l2);
                    black_box(l3);
                    black_box(l4);
                    black_box(l5);
                }
            }));
        }
        for _ in 0..20
        /*5*//*1*/
        {
            // let send = send.clone();
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                // let send = send.clone();
                for _ in 0..20000
                /*200*/
                {
                    tmp.store(Arc::new(rand::random()));
                }
            }));
        }
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
    });
}

/*
#[bench]
fn bench_other_multi(bencher: &mut Bencher) {
    let tmp = Arc::new(ArcSwap::new(Arc::new(0)));
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20/*5*//*1*/ {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..20000/*200*/ {
                    let l1 = tmp.load();
                    let l2 = tmp.load();
                    let l3 = tmp.load();
                    let l4 = tmp.load();
                    let l5 = tmp.load();
                    black_box(l1);
                    black_box(l2);
                    black_box(l3);
                    black_box(l4);
                    black_box(l5);
                }
            }));
        }
        for _ in 0..20/*5*//*1*/ {
            // let send = send.clone();
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                // let send = send.clone();
                for _ in 0..20000/*200*/ {
                    tmp.store(Arc::new(rand::random()));
                }
            }));
        }
        threads.into_iter().for_each(|thread| thread.join().unwrap());
    });
}*/

#[bench]
fn bench_us_single(bencher: &mut Bencher) {
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> =
        Arc::new(SwapArcIntermediateTLS::new(Arc::new(0)));
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..20000
                /*200*/
                {
                    let l1 = tmp.load();
                    black_box(l1);
                }
            }));
        }
        for _ in 0..20
        /*5*//*1*/
        {
            // let send = send.clone();
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                // let send = send.clone();
                for _ in 0..20000
                /*200*/
                {
                    tmp.store(Arc::new(rand::random()));
                }
            }));
        }
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
    });
}

/*
#[bench]
fn bench_other_single(bencher: &mut Bencher) {
    let tmp = Arc::new(ArcSwap::new(Arc::new(0)));
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20/*5*//*1*/ {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..20000/*200*/ {
                    let l1 = tmp.load();
                    black_box(l1);
                }
            }));
        }
        for _ in 0..20/*5*//*1*/ {
            // let send = send.clone();
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                // let send = send.clone();
                for _ in 0..20000/*200*/ {
                    tmp.store(Arc::new(rand::random()));
                }
            }));
        }
        threads.into_iter().for_each(|thread| thread.join().unwrap());
    });
}*/

#[bench]
fn bench_us_read_heavy_single(bencher: &mut Bencher) {
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> =
        Arc::new(SwapArcIntermediateTLS::new(Arc::new(3)));
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..200000
                /*200*/
                {
                    let l1 = tmp.load();
                    black_box(l1);
                }
            }));
        }
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
    });
}

#[bench]
fn bench_us_read_light_single(bencher: &mut Bencher) {
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> =
        Arc::new(SwapArcIntermediateTLS::new(Arc::new(3)));
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..1
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..200000
                /*200*/
                {
                    let l1 = tmp.load();
                    black_box(l1);
                }
            }));
        }
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
    });
}

#[bench]
fn bench_other_read_light_single(bencher: &mut Bencher) {
    let tmp = Arc::new(ArcSwap::new(Arc::new(3)));

    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..1
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..200000
                /*200*/
                {
                    let l1 = tmp.load();
                    black_box(l1);
                }
            }));
        }
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
    });
}

#[bench]
fn bench_other_read_heavy_single(bencher: &mut Bencher) {
    let tmp = Arc::new(ArcSwap::new(Arc::new(3)));

    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..200000
                /*200*/
                {
                    let l1 = tmp.load();
                    black_box(l1);
                }
            }));
        }
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
    });
}

#[bench]
fn bench_us_read_heavy_multi(bencher: &mut Bencher) {
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> =
        Arc::new(SwapArcIntermediateTLS::new(Arc::new(3)));
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..200000
                /*200*/
                {
                    let l1 = tmp.load();
                    let l2 = tmp.load();
                    let l3 = tmp.load();
                    let l4 = tmp.load();
                    let l5 = tmp.load();
                    black_box(l1);
                    black_box(l2);
                    black_box(l3);
                    black_box(l4);
                    black_box(l5);
                }
            }));
        }
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
    });
}

#[bench]
fn bench_other_read_heavy_multi(bencher: &mut Bencher) {
    let tmp = Arc::new(ArcSwap::new(Arc::new(3)));

    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..200000
                /*200*/
                {
                    let l1 = tmp.load();
                    let l2 = tmp.load();
                    let l3 = tmp.load();
                    let l4 = tmp.load();
                    let l5 = tmp.load();
                    black_box(l1);
                    black_box(l2);
                    black_box(l3);
                    black_box(l4);
                    black_box(l5);
                }
            }));
        }
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
    });
}

fn test_us_multi() {
    let arc = Arc::new(4);
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> =
        Arc::new(SwapArcIntermediateTLS::new(arc));
    tmp.store(Arc::new(31));

    let mut threads = vec![];
    for _ in 0..20
    /*5*//*1*/
    {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..20000
            /*200*/
            {
                let l1 = tmp.load();
                let l2 = tmp.load();
                let l3 = tmp.load();
                let l4 = tmp.load();
                let l5 = tmp.load();
                black_box(l1);
                black_box(l2);
                black_box(l3);
                black_box(l4);
                black_box(l5);
            }
        }));
    }
    for _ in 0..20
    /*5*//*1*/
    {
        // let send = send.clone();
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            // let send = send.clone();
            for _ in 0..20000
            /*200*/
            {
                tmp.store(Arc::new(rand::random()));
            }
        }));
    }
    threads
        .into_iter()
        .for_each(|thread| thread.join().unwrap());
}

fn test_us_single() {
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> =
        Arc::new(SwapArcIntermediateTLS::new(Arc::new(3)));
    let mut threads = vec![];
    for _ in 0..20
    /*5*//*1*/
    {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..2000
            /*200*/
            {
                let l1 = tmp.load();
                black_box(l1);
            }
        }));
    }
    for _ in 0..20
    /*5*//*1*/
    {
        // let send = send.clone();
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            // let send = send.clone();
            for _ in 0..2000
            /*200*/
            {
                tmp.store(Arc::new(rand::random()));
            }
        }));
    }
    threads
        .into_iter()
        .for_each(|thread| thread.join().unwrap());
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