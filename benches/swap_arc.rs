extern crate criterion;

use std::hint::{black_box, spin_loop};
use criterion::Criterion;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use swap_arc::auto_local_arc::AutoLocalArc;

fn main() {
    let mut c = Criterion::default().configure_from_args();

    c.bench_function("arc_read_heavy_single", |b| {
        let tmp = Arc::new(3);
        b.iter_custom(|iters| {
            let mut diff = Duration::default();
            for _ in 0..iters {
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
                        for _ in 0..200000
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
                let start = Instant::now();
                started.store(true, Ordering::Release);
                threads.into_iter().for_each(|thread| thread.join().unwrap());
                diff += start.elapsed();
            }
            diff
        });
    });
    c.bench_function("alarc_read_heavy_single", |b| {
        let tmp = AutoLocalArc::new(3);
        b.iter_custom(|iters| {
            let mut diff = Duration::default();
            for _ in 0..iters {
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
                        for _ in 0..200000
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
                let start = Instant::now();
                started.store(true, Ordering::Release);
                threads.into_iter().for_each(|thread| thread.join().unwrap());
                diff += start.elapsed();
            }
            diff
        });
    });
    c.bench_function("arc_read_light_single", |b| {
        let tmp = Arc::new(3);
        b.iter_custom(|iters| {
            let mut diff = Duration::default();
            for _ in 0..iters {
                let started = Arc::new(AtomicBool::new(false));
                let mut threads = vec![];
                for _ in 0..1
                {
                    let tmp = tmp.clone();
                    let started = started.clone();
                    threads.push(thread::spawn(move || {
                        while !started.load(Ordering::Acquire) {
                            spin_loop();
                        }
                        for _ in 0..200000
                        {
                            let l1 = tmp.clone();
                            black_box(l1);
                        }
                    }));
                }
                let start = Instant::now();
                started.store(true, Ordering::Release);
                threads.into_iter().for_each(|thread| thread.join().unwrap());
                diff += start.elapsed();
            }
            diff
        });
    });
    c.bench_function("alarc_read_light_single", |b| {
        let tmp = AutoLocalArc::new(3);
        b.iter_custom(|iters| {
            let mut diff = Duration::default();
            for _ in 0..iters {
                let started = Arc::new(AtomicBool::new(false));
                let mut threads = vec![];
                for _ in 0..1
                {
                    let tmp = tmp.clone();
                    let started = started.clone();
                    threads.push(thread::spawn(move || {
                        while !started.load(Ordering::Acquire) {
                            spin_loop();
                        }
                        for _ in 0..200000
                        {
                            let l1 = tmp.clone();
                            black_box(l1);
                        }
                    }));
                }
                let start = Instant::now();
                started.store(true, Ordering::Release);
                threads.into_iter().for_each(|thread| thread.join().unwrap());
                diff += start.elapsed();
            }
            diff
        });
    });
    c.bench_function("arc_read_light_multi", |b| {
        let tmp = Arc::new(3);
        b.iter_custom(|iters| {
            let mut diff = Duration::default();
            for _ in 0..iters {
                let started = Arc::new(AtomicBool::new(false));
                let mut threads = vec![];
                for _ in 0..20
                {
                    let tmp = tmp.clone();
                    let started = started.clone();
                    threads.push(thread::spawn(move || {
                        while !started.load(Ordering::Acquire) {
                            spin_loop();
                        }
                        for _ in 0..200000
                        {
                            let l1 = tmp.clone();
                            black_box(l1);
                        }
                    }));
                }
                let start = Instant::now();
                started.store(true, Ordering::Release);
                threads.into_iter().for_each(|thread| thread.join().unwrap());
                diff += start.elapsed();
            }
            diff
        });
    });
    c.bench_function("alarc_read_light_multi", |b| {
        let tmp = AutoLocalArc::new(3);
        b.iter_custom(|iters| {
            let mut diff = Duration::default();
            for _ in 0..iters {
                let started = Arc::new(AtomicBool::new(false));
                let mut threads = vec![];
                for _ in 0..20
                {
                    let tmp = tmp.clone();
                    let started = started.clone();
                    threads.push(thread::spawn(move || {
                        while !started.load(Ordering::Acquire) {
                            spin_loop();
                        }
                        for _ in 0..200000
                        {
                            let l1 = tmp.clone();
                            black_box(l1);
                        }
                    }));
                }
                let start = Instant::now();
                started.store(true, Ordering::Release);
                threads.into_iter().for_each(|thread| thread.join().unwrap());
                diff += start.elapsed();
            }
            diff
        });
    });
    c.bench_function("arc_read_heavy_multi", |b| {
        let tmp = Arc::new(3);
        b.iter_custom(|iters| {
            let mut diff = Duration::default();
            for _ in 0..iters {
                let started = Arc::new(AtomicBool::new(false));
                let mut threads = vec![];
                for _ in 0..20
                {
                    let tmp = tmp.clone();
                    let started = started.clone();
                    threads.push(thread::spawn(move || {
                        while !started.load(Ordering::Acquire) {
                            spin_loop();
                        }
                        for _ in 0..200000
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
                let start = Instant::now();
                started.store(true, Ordering::Release);
                threads.into_iter().for_each(|thread| thread.join().unwrap());
                diff += start.elapsed();
            }
            diff
        });
    });
    c.bench_function("alarc_read_heavy_multi", |b| {
        let tmp = AutoLocalArc::new(3);
        b.iter_custom(|iters| {
            let mut diff = Duration::default();
            for _ in 0..iters {
                let started = Arc::new(AtomicBool::new(false));
                let mut threads = vec![];
                for _ in 0..20
                {
                    let tmp = tmp.clone();
                    let started = started.clone();
                    threads.push(thread::spawn(move || {
                        while !started.load(Ordering::Acquire) {
                            spin_loop();
                        }
                        for _ in 0..200000
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
                let start = Instant::now();
                started.store(true, Ordering::Release);
                threads.into_iter().for_each(|thread| thread.join().unwrap());
                diff += start.elapsed();
            }
            diff
        });
    });
    c.bench_function("arc_read_light_single_many", |b| {
        let tmp = Arc::new(3);
        b.iter_custom(|iters| {
            let mut diff = Duration::default();
            for _ in 0..iters {
                let started = Arc::new(AtomicBool::new(false));
                let mut threads = vec![];
                for _ in 0..20
                {
                    let tmp = tmp.clone();
                    let started = started.clone();
                    threads.push(thread::spawn(move || {
                        while !started.load(Ordering::Acquire) {
                            spin_loop();
                        }
                        for _ in 0..200000
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
                let start = Instant::now();
                started.store(true, Ordering::Release);
                threads.into_iter().for_each(|thread| thread.join().unwrap());
                diff += start.elapsed();
            }
            diff
        });
    });
    c.bench_function("alarc_read_light_single_many", |b| {
        let tmp = AutoLocalArc::new(3);
        b.iter_custom(|iters| {
            let mut diff = Duration::default();
            for _ in 0..iters {
                let started = Arc::new(AtomicBool::new(false));
                let mut threads = vec![];
                for _ in 0..20
                {
                    let tmp = tmp.clone();
                    let started = started.clone();
                    threads.push(thread::spawn(move || {
                        while !started.load(Ordering::Acquire) {
                            spin_loop();
                        }
                        for _ in 0..200000
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
                let start = Instant::now();
                started.store(true, Ordering::Release);
                threads.into_iter().for_each(|thread| thread.join().unwrap());
                diff += start.elapsed();
            }
            diff
        });
    });
    c.bench_function("create_arc", |b| {
        b.iter(|| {
            black_box(Arc::new(3));
        });
    });
    c.bench_function("create_alarc", |b| {
        b.iter(|| {
            black_box(AutoLocalArc::new(3));
        });
    });
}

/*#[bench]
fn bench_abrc_read_light_single_many(bencher: &mut Bencher) {
    let tmp = Arc::new(3);
    bencher.iter(|| {
        for _ in 0..1000 {
            let mut threads = vec![];
            for _ in 0..1
            /*5*//*1*/
            {
                let tmp = tmp.clone();
                threads.push(thread::spawn(move || {
                    for _ in 0..20
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
        }
    });
}*/

/*#[bench]
fn bench_alarc_read_light_single_many(bencher: &mut Bencher) {
    let tmp = AutoLocalArc::new(3);
    bencher.iter(|| {
        for _ in 0..1000 {
            let mut threads = vec![];
            for _ in 0..1
            /*5*//*1*/
            {
                let tmp = tmp.clone();
                threads.push(thread::spawn(move || {
                    for _ in 0..20
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
        }
    });
}*/