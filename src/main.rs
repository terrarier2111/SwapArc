#![feature(core_intrinsics)]
#![feature(arbitrary_self_types)]
// only for testing!
#![allow(soft_unstable)]
#![feature(test)]
#![feature(bench_black_box)]
#![feature(thread_id_value)]
#![feature(pointer_byte_offsets)]
#![feature(thread_local)]

mod auto_local_arc;
mod cached_arc;
mod spreaded_arc;
mod swap_arc_tls_optimistic;

use crate::swap_arc_tls_optimistic::SwapArcIntermediateTLS;
use std::hint::{black_box, spin_loop};
use std::mem::transmute;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{mem, thread};
use std::backtrace::Backtrace;

#[cfg(test)]
extern crate test;
use crate::auto_local_arc::AutoLocalArc;
#[cfg(test)]
use arc_swap::ArcSwap;
#[cfg(test)]
use test::Bencher;

pub(crate) static TID: AtomicUsize = AtomicUsize::new(0);

fn main() {
    /*for _ in 0..10 {
        test_us_single();
    }*/
    /*for _ in 0..10 {
        test_us_multi();
    }*/
    // bad_bench_us_multi();
    /*
    let arc = Arc::new(4);
    let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> = SwapArcIntermediateTLS::new(arc);
    let mut threads = vec![];
    tmp.update(Arc::new(31));
    println!("{}", tmp.load());
    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    for _ in 0..20/*5*//*1*/ {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for x in 0..2000/*200*/ {
                let l1 = tmp.load();
                let l2 = tmp.load();
                let l3 = tmp.load();
                let l4 = tmp.load();
                let l5 = tmp.load();
                println!("{}{}{}{}{}", l1, l2, l3, l4, l5);
                if x % 5 == 0 {
                    println!("completed load: {x}");
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
            for x in 0..2000/*200*/ {
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
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    println!("test took: {}ms", time - start);
    // loop {}
    println!("{:?}", tmp.load().as_ref());
    tmp.update(Arc::new(6));
    println!("{:?}", tmp.load().as_ref());
    println!("test! 0");*/
    /*let tmp: Arc<SwapArcIntermediateTLS<i32, Arc<i32>, 0>> = SwapArcIntermediateTLS::new(Arc::new(0));
    let mut threads = vec![];
    for _ in 0..20/*5*//*1*/ {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..2000/*200*/ {
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
            for _ in 0..2000/*200*/ {
                tmp.update(Arc::new(rand::random()));
            }
        }));
    }
    threads.into_iter().for_each(|thread| thread.join().unwrap());*/
    // let tmp = Arc::new(ArcSwap::new(Arc::new(3)));
    // let tmp = AutoLocalArc::new(3);
    /*let tmp = AutoLocalArc::new(3);
    let mut threads = vec![];
    for _ in 0../*1*//*6*//*8*/7
    /*20*//*5*//*1*/
    {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..400
            /*200000*//*200*/
            {
                let l1 = tmp.clone();
                black_box(l1);
            }
            /*let tmp1 = tmp.clone();
            thread::spawn(move || {
                /*tmp*/
                black_box(tmp1.clone());
            })*/
        }));
    }
    // drop(tmp);
    println!("awaiting stuff!");
    threads
        .into_iter()
        .for_each(|thread| thread.join()/*.unwrap().join()*/.unwrap());
    thread::sleep(Duration::from_secs(10));*/
    TID.store(thread_id::get(), Ordering::Release);
    let tmp = AutoLocalArc::new(3);
    let mut threads = vec![];
    for _ in 0..10/*10*/
    /*5*//*1*/
    {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..2000
            /*200*/
            {
                let l1 = tmp.clone();
                black_box(l1);
            }
        }));
    }
    let mut fin = 0;
    threads
        .into_iter()
        .for_each(|thread| {
            /*if !thread.is_finished() {
                thread::sleep(Duration::from_millis(1000));
                if !thread.is_finished() {

                }
            }*/
            thread.join().unwrap();
            println!("finished thread: {}", fin);
            fin += 1;
        });
    println!("finished all!");
    thread::sleep(Duration::from_millis(50));





    /*let mut threads = vec![];
    for _ in 0..5/*20*//*5*//*1*/ {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..200/*20000*//*200*/ {
                /*let l1 = tmp.load();
                let l2 = tmp.load();
                let l3 = tmp.load();
                let l4 = tmp.load();
                let l5 = tmp.load();
                black_box(l1);
                black_box(l2);
                black_box(l3);
                black_box(l4);
                black_box(l5);*/
                let l1 = tmp.clone();
                black_box(l1);
            }
        }));
    }
    /*for _ in 0..20/*5*//*1*/ {
        let tmp = tmp.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..20000/*200*/ {
                tmp.update(Arc::new(rand::random()));
            }
        }));
    }*/
    threads.into_iter().for_each(|thread| thread.join().unwrap());*/
    /*let tmp1 = tmp.clone();
    thread::spawn(move || {
        let l1 = tmp1.clone();
        black_box(l1);
    });
    for _ in 0..200/*20000*//*200*/ {
        /*let l1 = tmp.load();
        let l2 = tmp.load();
        let l3 = tmp.load();
        let l4 = tmp.load();
        let l5 = tmp.load();
        black_box(l1);
        black_box(l2);
        black_box(l3);
        black_box(l4);
        black_box(l5);*/
        let l1 = tmp.clone();
        black_box(l1);
    }*/
}

/*
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
/*
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
}*/

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

/*
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
}*/

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
}*/

#[bench]
fn bench_arc_read_heavy_single(bencher: &mut Bencher) {
    let tmp = Arc::new(3);
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
                    let l1 = tmp.clone();
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
fn bench_arc_read_light_single(bencher: &mut Bencher) {
    let tmp = Arc::new(3);
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
                    let l1 = tmp.clone();
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
fn bench_arc_read_heavy_multi(bencher: &mut Bencher) {
    let tmp = Arc::new(3);
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20/*5*//*1*/ {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..200000/*200*/ {
                    let l1 = tmp.clone();
                    let l2 = tmp.clone();
                    let l3 = tmp.clone();
                    let l4 = tmp.clone();
                    let l5 = tmp.clone();
                    black_box(l1);
                    black_box(l2);
                    black_box(l3);
                    black_box(l4);
                    black_box(l5);
                }
            }));
        }
        threads.into_iter().for_each(|thread| thread.join().unwrap());
    });
}

#[bench]
fn bench_arc_read_light_multi(bencher: &mut Bencher) {
    let tmp = Arc::new(3);
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
                    let l1 = tmp.clone();
                    let l2 = tmp.clone();
                    let l3 = tmp.clone();
                    let l4 = tmp.clone();
                    let l5 = tmp.clone();
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

/*
#[bench]
fn bench_alarc_read_heavy_single(bencher: &mut Bencher) {
    let tmp = AutoLocalArc::new(3);
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
                    let l1 = tmp.clone();
                    black_box(l1);
                }
            }));
        }
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
    });
}*/

#[bench]
fn bench_alarc_read_light_single(bencher: &mut Bencher) {
    let tmp = AutoLocalArc::new(3);
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
                    let l1 = tmp.clone();
                    black_box(l1);
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
fn bench_alarc_read_heavy_multi(bencher: &mut Bencher) {
    let tmp = AutoLocalArc::new(3);
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
                    let l1 = tmp.clone();
                    let l2 = tmp.clone();
                    let l3 = tmp.clone();
                    let l4 = tmp.clone();
                    let l5 = tmp.clone();
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
}*/

#[bench]
fn bench_alarc_read_light_multi(bencher: &mut Bencher) {
    let tmp = AutoLocalArc::new(3);
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
                    let l1 = tmp.clone();
                    let l2 = tmp.clone();
                    let l3 = tmp.clone();
                    let l4 = tmp.clone();
                    let l5 = tmp.clone();
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

/*
#[bench]
fn bench_cached_read_heavy_single(bencher: &mut Bencher) {
    let tmp = CachedArc::new(3);
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
                    let l1 = tmp.clone();
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
fn bench_cached_read_light_single(bencher: &mut Bencher) {
    let tmp = CachedArc::new(3);
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
                    let l1 = tmp.clone();
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
fn bench_cached_read_heavy_multi_local(bencher: &mut Bencher) {
    let tmp = CachedArc::new(3);
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
                    let l1 = tmp.downgrade_cloned();
                    let l2 = l1.clone();
                    let l3 = l1.clone();
                    let l4 = l1.clone();
                    let l5 = l1.clone();
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
fn bench_cached_read_light_multi_local(bencher: &mut Bencher) {
    let tmp = CachedArc::new(3);
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
                    let l1 = tmp.downgrade_cloned();
                    let l2 = l1.clone();
                    let l3 = l1.clone();
                    let l4 = l1.clone();
                    let l5 = l1.clone();
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
fn bench_spreaded_read_heavy_single(bencher: &mut Bencher) {
    let tmp = SpreadedArc::new(3);
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                let tmp = tmp.clone(); // this line probably changes perf
                for _ in 0..200000
                /*200*/
                {
                    let l1 = tmp.clone();
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
fn bench_spreaded_read_light_single(bencher: &mut Bencher) {
    let tmp = SpreadedArc::new(3);
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..1
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                let tmp = tmp.clone(); // this line probably changes perf
                for _ in 0..200000
                /*200*/
                {
                    let l1 = tmp.clone();
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
fn bench_spreaded_read_heavy_multi_local(bencher: &mut Bencher) {
    let tmp = SpreadedArc::new(3);
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..20
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                let tmp = tmp.clone(); // this line probably changes perf
                for _ in 0..200000
                /*200*/
                {
                    let l1 = tmp.clone();
                    let l2 = l1.clone();
                    let l3 = l1.clone();
                    let l4 = l1.clone();
                    let l5 = l1.clone();
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
fn bench_spreaded_read_light_multi_local(bencher: &mut Bencher) {
    let tmp = SpreadedArc::new(3);
    bencher.iter(|| {
        let mut threads = vec![];
        for _ in 0..1
        /*5*//*1*/
        {
            let tmp = tmp.clone();
            threads.push(thread::spawn(move || {
                let tmp = tmp.clone(); // this line probably changes perf
                for _ in 0..200000
                /*200*/
                {
                    let l1 = tmp.clone();
                    let l2 = l1.clone();
                    let l3 = l1.clone();
                    let l4 = l1.clone();
                    let l5 = l1.clone();
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
*/

/*
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
}*/

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
