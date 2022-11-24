# SwapArc

SwapArc allows you to swap out Arcs while using them. \
Let's consider this example:

```rust
use std::sync::Arc;
use std::thread;

struct Config {
    timout: u64,
    ...
}

struct Server {
    config: SwapArc<Config>,
    ...
}

fn test() {
    let server = Arc::new(Server {
        config: Config {
            timout: 1000,
            ...
        },
        ...
    });
    thread::spawn(|| {
        loop {
            // load the config without fearing any blocking or expensive operations.
            server.accept_connection(server.config.load().timeout);
        }
    });
    ...
}

// on network update, update the config seamlessly without blocking loads
server.config.update(Arc::new(Config {
    timeout: ...,
    ...
}));
```

## Contents
- Why?
- Usage
- Benchmarks

### Why?

..

### Usage

..

### Benchmarks

``Date: 09.11.2022``

<details><summary>CPU: AMD Ryzen 5 3600 6-Core Processor</summary>
<p>

```jsx
test bench_other_multi             ... bench: 337,077,090 ns/iter (+/- 13,378,624)
test bench_other_read_heavy_multi  ... bench:  26,604,620 ns/iter (+/- 2,268,690)
test bench_other_read_heavy_single ... bench:   4,744,525 ns/iter (+/- 347,515)
test bench_other_single            ... bench: 338,127,170 ns/iter (+/- 12,416,516)
test bench_us_multi                ... bench:  29,804,930 ns/iter (+/- 1,830,429)
test bench_us_read_heavy_multi     ... bench:  24,220,320 ns/iter (+/- 410,629)
test bench_us_read_heavy_single    ... bench:   4,989,780 ns/iter (+/- 128,699)
test bench_us_single               ... bench:  25,904,060 ns/iter (+/- 973,951)
```

</p>
</details>

``Date: 11.11.2022``

<details><summary>CPU: AMD Ryzen 5 3600 6-Core Processor</summary>
<p>

```jsx
test bench_other_multi             ... bench: 286,886,920 ns/iter (+/- 31,744,594)
test bench_other_read_heavy_multi  ... bench:  26,334,210 ns/iter (+/- 1,016,207)
test bench_other_read_heavy_single ... bench:   4,599,380 ns/iter (+/- 188,476)
test bench_other_single            ... bench: 284,633,540 ns/iter (+/- 6,663,107)
test bench_us_multi                ... bench:  28,592,890 ns/iter (+/- 1,699,613)
test bench_us_read_heavy_multi     ... bench:  21,874,820 ns/iter (+/- 203,017)
test bench_us_read_heavy_single    ... bench:   4,388,770 ns/iter (+/- 74,011)
test bench_us_single               ... bench:  25,560,430 ns/iter (+/- 1,717,332)
```

```jsx
test bench_other_multi             ... bench: 281,094,410 ns/iter (+/- 8,515,881)
test bench_other_read_heavy_multi  ... bench:  26,095,060 ns/iter (+/- 555,046)
test bench_other_read_heavy_single ... bench:   4,605,400 ns/iter (+/- 333,495)
test bench_other_single            ... bench: 278,551,080 ns/iter (+/- 6,233,614)
test bench_us_multi                ... bench:  26,216,680 ns/iter (+/- 806,433)
test bench_us_read_heavy_multi     ... bench:  21,806,780 ns/iter (+/- 261,765)
test bench_us_read_heavy_single    ... bench:   4,373,490 ns/iter (+/- 99,582)
test bench_us_single               ... bench:  25,527,940 ns/iter (+/- 1,576,593)
```

```jsx
test bench_other_multi             ... bench: 286,954,300 ns/iter (+/- 42,713,448)
test bench_other_read_heavy_multi  ... bench:  26,317,190 ns/iter (+/- 748,813)
test bench_other_read_heavy_single ... bench:   4,635,690 ns/iter (+/- 226,496)
test bench_other_single            ... bench: 291,112,510 ns/iter (+/- 8,639,937)
test bench_us_multi                ... bench:  28,405,350 ns/iter (+/- 1,223,287)
test bench_us_read_heavy_multi     ... bench:  21,873,610 ns/iter (+/- 577,473)
test bench_us_read_heavy_single    ... bench:   4,418,590 ns/iter (+/- 804,933)
test bench_us_single               ... bench:  25,439,660 ns/iter (+/- 1,911,437)
```

```jsx
test bench_other_multi             ... bench: 259,917,990 ns/iter (+/- 14,681,041)
test bench_other_read_heavy_multi  ... bench:  26,056,590 ns/iter (+/- 738,703)
test bench_other_read_heavy_single ... bench:   4,587,115 ns/iter (+/- 195,985)
test bench_other_single            ... bench: 260,428,840 ns/iter (+/- 4,843,635)
test bench_us_multi                ... bench:  28,347,380 ns/iter (+/- 1,399,337)
test bench_us_read_heavy_multi     ... bench:  21,812,110 ns/iter (+/- 178,127)
test bench_us_read_heavy_single    ... bench:   4,378,230 ns/iter (+/- 68,019)
test bench_us_single               ... bench:  26,188,510 ns/iter (+/- 1,103,597)
```

</p>
</details>

<details><summary>CPU: AMD Ryzen 9 5900X 12-Core Processor</summary>
<p>

```jsx
test bench_other_multi             ... bench: 184,017,011 ns/iter (+/- 7,380,849)
test bench_other_read_heavy_multi  ... bench: 7,245,057 ns/iter (+/- 783,988)
test bench_other_read_heavy_single ... bench: 1,633,602 ns/iter (+/- 211,810)
test bench_other_single            ... bench: 188,822,041 ns/iter (+/- 8,014,960)
test bench_us_multi                ... bench: 12,212,070 ns/iter (+/- 563,605)
test bench_us_read_heavy_multi     ... bench: 6,732,108 ns/iter (+/- 2,508,756)
test bench_us_read_heavy_single    ... bench: 1,475,758 ns/iter (+/- 196,761)
test bench_us_single               ... bench: 11,651,460 ns/iter (+/- 366,257)
```

</p>
</details>

---

<details><summary>CPU: Intel i3-3110M (4) @ 2.4GHz</summary>
<p>

```jsx
test bench_other_multi             ... bench: 1,448,044,362 ns/iter (+/- 95,468,062)
test bench_other_read_heavy_multi  ... bench: 218,297,447 ns/iter (+/- 24,643,900)
test bench_other_read_heavy_single ... bench: 42,376,534 ns/iter (+/- 3,832,873)
test bench_other_single            ... bench: 1,428,660,946 ns/iter (+/- 77,924,373)
test bench_us_multi                ... bench: 43,881,707 ns/iter (+/- 3,118,934)
test bench_us_read_heavy_multi     ... bench: 99,083,670 ns/iter (+/- 9,962,635)
test bench_us_read_heavy_single    ... bench: 19,858,851 ns/iter (+/- 2,966,487)
test bench_us_single               ... bench: 33,062,356 ns/iter (+/- 2,483,705) 
```

</p>
</details>

<details><summary>CPU: Intel(R) Core(TM) i3-8350K CPU @ 4.00GHz</summary>
<p>

```jsx
test bench_other_multi             ... bench: 455,407,497 ns/iter (+/- 105,855,108)
test bench_other_read_heavy_multi  ... bench: 75,012,985 ns/iter (+/- 24,828,287)
test bench_other_read_heavy_single ... bench: 15,306,724 ns/iter (+/- 3,694,570)
test bench_other_single            ... bench: 445,768,474 ns/iter (+/- 41,304,643)
test bench_us_multi                ... bench: 33,029,626 ns/iter (+/- 4,227,672)
test bench_us_read_heavy_multi     ... bench: 36,337,792 ns/iter (+/- 3,656,744)
test bench_us_read_heavy_single    ... bench: 7,816,087 ns/iter (+/- 951,896)
test bench_us_single               ... bench: 25,390,250 ns/iter (+/- 3,131,246) 
```

```jsx
test bench_other_multi             ... bench: 415,349,961 ns/iter (+/- 2,692,847)
test bench_other_read_heavy_multi  ... bench: 67,739,327 ns/iter (+/- 1,781,835)
test bench_other_read_heavy_single ... bench: 14,646,336 ns/iter (+/- 1,042,389)
test bench_other_single            ... bench: 409,009,813 ns/iter (+/- 23,341,306)
test bench_us_multi                ... bench: 32,178,438 ns/iter (+/- 2,969,374)
test bench_us_read_heavy_multi     ... bench: 31,844,988 ns/iter (+/- 1,063,819)
test bench_us_read_heavy_single    ... bench: 7,314,487 ns/iter (+/- 473,510)
test bench_us_single               ... bench: 27,780,288 ns/iter (+/- 1,178,594)
```

```jsx
test bench_other_multi             ... bench: 455,367,374 ns/iter (+/- 34,010,803)
test bench_other_read_heavy_multi  ... bench: 71,258,479 ns/iter (+/- 14,277,451)
test bench_other_read_heavy_single ... bench: 14,738,168 ns/iter (+/- 1,603,971)
test bench_other_single            ... bench: 452,809,517 ns/iter (+/- 35,419,075)
test bench_us_multi                ... bench: 29,255,190 ns/iter (+/- 4,420,725)
test bench_us_read_heavy_multi     ... bench: 25,457,460 ns/iter (+/- 2,809,383)
test bench_us_read_heavy_single    ... bench: 5,675,088 ns/iter (+/- 543,492)
test bench_us_single               ... bench: 24,220,657 ns/iter (+/- 1,698,348) 
```

</p>
</details>    

``Date: 21.11.2022``

<details><summary>CPU: AMD Ryzen 7 2700 (16) @ 3.200GHz </summary>
<p>

```jsx
test bench_arc_read_heavy_single   ... bench: 113,045,390 ns/iter (+/- 3,037,919)
test bench_arc_read_light_single   ... bench:   5,993,131 ns/iter (+/- 1,097,352)
test bench_other_read_heavy_multi  ... bench:  42,352,095 ns/iter (+/- 10,582,939)
test bench_other_read_heavy_single ... bench:   7,240,837 ns/iter (+/- 225,284)
test bench_other_read_light_single ... bench:   7,513,911 ns/iter (+/- 2,107,815)
test bench_us_multi                ... bench:  24,163,411 ns/iter (+/- 1,379,801)
test bench_us_read_heavy_multi     ... bench:  24,132,540 ns/iter (+/- 6,651,946)
test bench_us_read_heavy_single    ... bench:   4,151,779 ns/iter (+/- 352,967)
test bench_us_read_light_single    ... bench:   2,646,784 ns/iter (+/- 444,961)
test bench_us_single               ... bench:  18,519,491 ns/iter (+/- 703,753)
```    
    
</p>
</details>


<details><summary>CPU: Intel i5-7200U (4) @ 3.100GHz </summary>
<p>

```jsx
test bench_arc_read_heavy_single   ... bench: 151,675,598 ns/iter (+/- 22,690,925)
test bench_arc_read_light_single   ... bench:   2,526,499 ns/iter (+/- 116,747)
test bench_other_read_heavy_multi  ... bench: 145,097,272 ns/iter (+/- 6,514,778)
test bench_other_read_heavy_single ... bench:  32,727,815 ns/iter (+/- 4,309,793)
test bench_other_read_light_single ... bench:   3,696,212 ns/iter (+/- 169,270)
test bench_us_multi                ... bench:  36,482,013 ns/iter (+/- 2,250,393)
test bench_us_read_heavy_multi     ... bench:  62,337,136 ns/iter (+/- 6,026,757)
test bench_us_read_heavy_single    ... bench:  14,542,977 ns/iter (+/- 2,224,343)
test bench_us_read_light_single    ... bench:   1,429,254 ns/iter (+/- 289,592)
test bench_us_single               ... bench:  29,470,658 ns/iter (+/- 2,413,649)
```    
    
</p>
</details>
