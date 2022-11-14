extern crate criterion;

use std::ops::Sub;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use criterion::Bencher;

use thread_local::ThreadLocal;
use swap_arc::SwapArc;

/*
fn main() {
    let mut c = criterion::Criterion::default().configure_from_args();
    benchmark(&mut c);
}

pub fn benchmark(c: &mut Criterion) {
    c.bench_function("load", |b| {
        b.iter_custom(|bench| {
            let shared = Arc::new(SwapArc::new(Arc::new(3)));
            let start = SystemTime::now();
            let start = start
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards");
            let mut threads = vec![];
            for _ in 0..20/*5*//*1*/ {
                let tmp = shared.clone();
                threads.push(thread::spawn(move || {
                    for _ in 0..200000/*200*/ {
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
            threads.into_iter().for_each(|thread| thread.join().unwrap());
            let end = SystemTime::now();
            let end = end
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards");
            end.sub(start)
        })
    });

    c.bench_function("insert", |b| {
        b.iter_custom(|bench| {
            let shared = Arc::new(SwapArc::new(Arc::new(3)));
            let start = SystemTime::now();
            let start = start
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards");
            let mut threads = vec![];
            for _ in 0..20/*5*//*1*/ {
                let tmp = shared.clone();
                threads.push(thread::spawn(move || {
                    for _ in 0..200000/*200*/ {
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
            threads.into_iter().for_each(|thread| thread.join().unwrap());
            let end = SystemTime::now();
            let end = end
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards");
            end.sub(start)
        })
    });
}*/