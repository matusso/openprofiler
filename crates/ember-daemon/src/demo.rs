//! A deterministic synthetic profile of a small web shop, used by `ember daemon --demo`, UI
//! development and tests.

use ember_model::*;

const NS: i64 = 1_000_000_000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

type Path = &'static [(&'static str, &'static str, &'static str)];

const SERVLET: Path = &[
    ("java.lang.Thread", "run", "()V"),
    ("java.util.concurrent.ThreadPoolExecutor$Worker", "run", "()V"),
    ("org.apache.tomcat.util.net.NioEndpoint$SocketProcessor", "doRun", "()V"),
    ("org.apache.catalina.core.StandardWrapperValve", "invoke", "(Lorg/apache/catalina/connector/Request;)V"),
    (
        "jakarta.servlet.http.HttpServlet",
        "service",
        "(Ljakarta/servlet/ServletRequest;Ljakarta/servlet/ServletResponse;)V",
    ),
    ("org.springframework.web.servlet.DispatcherServlet", "doDispatch", "()V"),
];

/// Leaf paths below the servlet prefix with relative weights and the thread state they run in.
const LEAVES: &[(u64, ThreadState, Path)] = &[
    (
        30,
        ThreadState::Runnable,
        &[
            (
                "dev.ember.shop.OrderController",
                "placeOrder",
                "(Ldev/ember/shop/OrderRequest;)Ldev/ember/shop/Order;",
            ),
            ("dev.ember.shop.OrderService", "price", "(Ldev/ember/shop/Order;)Ljava/math/BigDecimal;"),
            ("dev.ember.shop.PricingEngine", "applyRules", "(Ljava/util/List;)V"),
            ("dev.ember.shop.PricingEngine", "matchRule", "(Ldev/ember/shop/Rule;)Z"),
            (
                "java.util.regex.Pattern$Start",
                "match",
                "(Ljava/util/regex/Matcher;ILjava/lang/CharSequence;)Z",
            ),
        ],
    ),
    (
        12,
        ThreadState::Runnable,
        &[
            (
                "dev.ember.shop.OrderController",
                "placeOrder",
                "(Ldev/ember/shop/OrderRequest;)Ldev/ember/shop/Order;",
            ),
            ("dev.ember.shop.OrderService", "price", "(Ldev/ember/shop/Order;)Ljava/math/BigDecimal;"),
            ("java.math.BigDecimal", "multiply", "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;"),
        ],
    ),
    (
        18,
        ThreadState::NetIo,
        &[
            (
                "dev.ember.shop.OrderController",
                "placeOrder",
                "(Ldev/ember/shop/OrderRequest;)Ldev/ember/shop/Order;",
            ),
            ("dev.ember.shop.OrderRepository", "save", "(Ldev/ember/shop/Order;)V"),
            (
                "org.springframework.jdbc.core.JdbcTemplate",
                "update",
                "(Ljava/lang/String;[Ljava/lang/Object;)I",
            ),
            ("org.postgresql.jdbc.PgPreparedStatement", "executeUpdate", "()I"),
            ("java.net.SocketInputStream", "socketRead0", "(Ljava/io/FileDescriptor;[BIII)I"),
        ],
    ),
    (
        14,
        ThreadState::Runnable,
        &[
            ("dev.ember.shop.CatalogController", "search", "(Ljava/lang/String;)Ljava/util/List;"),
            ("dev.ember.shop.CatalogService", "search", "(Ljava/lang/String;)Ljava/util/List;"),
            ("dev.ember.shop.SearchIndex", "query", "(Ljava/lang/String;)Ljava/util/List;"),
            ("java.util.HashMap", "get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java.lang.String", "hashCode", "()I"),
        ],
    ),
    (
        9,
        ThreadState::Runnable,
        &[
            ("dev.ember.shop.CatalogController", "search", "(Ljava/lang/String;)Ljava/util/List;"),
            ("com.fasterxml.jackson.databind.ObjectMapper", "writeValueAsBytes", "(Ljava/lang/Object;)[B"),
            ("com.fasterxml.jackson.core.json.UTF8JsonGenerator", "writeString", "(Ljava/lang/String;)V"),
        ],
    ),
    (
        8,
        ThreadState::Blocked,
        &[
            ("dev.ember.shop.CatalogController", "search", "(Ljava/lang/String;)Ljava/util/List;"),
            ("dev.ember.shop.ProductCache", "get", "(J)Ldev/ember/shop/Product;"),
        ],
    ),
    (
        6,
        ThreadState::Waiting,
        &[
            (
                "dev.ember.shop.OrderController",
                "placeOrder",
                "(Ldev/ember/shop/OrderRequest;)Ldev/ember/shop/Order;",
            ),
            ("com.zaxxer.hikari.pool.HikariPool", "getConnection", "(J)Ljava/sql/Connection;"),
            ("java.util.concurrent.locks.LockSupport", "parkNanos", "(Ljava/lang/Object;J)V"),
        ],
    ),
    (
        5,
        ThreadState::Runnable,
        &[
            ("dev.ember.shop.ReportController", "export", "()V"),
            ("dev.ember.shop.ReportService", "fib", "(I)J"),
            ("dev.ember.shop.ReportService", "fib", "(I)J"),
            ("dev.ember.shop.ReportService", "fib", "(I)J"),
            ("dev.ember.shop.ReportService", "fib", "(I)J"),
        ],
    ),
];

const ALLOC: &[(u64, &str, Path)] = &[
    (
        40,
        "byte[]",
        &[("com.fasterxml.jackson.databind.ObjectMapper", "writeValueAsBytes", "(Ljava/lang/Object;)[B")],
    ),
    (
        25,
        "java.lang.String",
        &[("dev.ember.shop.SearchIndex", "query", "(Ljava/lang/String;)Ljava/util/List;")],
    ),
    (
        20,
        "java.math.BigDecimal",
        &[("java.math.BigDecimal", "multiply", "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;")],
    ),
    (
        15,
        "dev.ember.shop.Order",
        &[(
            "dev.ember.shop.OrderController",
            "placeOrder",
            "(Ldev/ember/shop/OrderRequest;)Ldev/ember/shop/Order;",
        )],
    ),
];

fn intern_path(p: &mut Profile, prefix: Path, path: Path) -> StackId {
    let mut frames = Vec::new();
    for (i, (c, m, s)) in prefix.iter().chain(path.iter()).enumerate() {
        let mid = p.symbols.intern_method(c, m, s);
        let kind = if c.starts_with("java.net") && m.ends_with('0') {
            FrameKind::Native
        } else if i % 3 == 2 {
            FrameKind::Inlined
        } else {
            FrameKind::Jit
        };
        frames.push(p.symbols.intern_frame(mid, 40 + i as i32 * 7, kind));
    }
    p.stacks.intern_root_first(frames)
}

/// Builds a 60 second profile with 9 threads.
pub fn demo_profile() -> Profile {
    let mut p = Profile::new("demo");
    let start: Nanos = 1_760_000_000 * NS;
    let duration = 60 * NS;
    let interval = 10_000_000; // 10 ms
    p.meta.start = start;
    p.meta.end = start + duration;
    p.meta.cpu_interval = Some(interval);
    p.meta.jvm = JvmInfo {
        pid: Some(4242),
        name: "dev.ember.shop.ShopApplication".into(),
        vm_name: "OpenJDK 64-Bit Server VM".into(),
        vm_version: "21.0.4+7-LTS".into(),
        vm_vendor: "Eclipse Adoptium".into(),
        java_version: "21.0.4".into(),
        jvm_args: vec!["-Xmx2g".into(), "-XX:+UseG1GC".into()],
        main_class: "dev.ember.shop.ShopApplication".into(),
        os: "Linux 6.8".into(),
        arch: "aarch64".into(),
        cpus: Some(8),
        max_heap: Some(2 << 30),
        start_time: Some(start - 300 * NS),
    };
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    let mut threads = vec![p.thread_index(1, || "main".into())];
    for i in 1..=8u64 {
        threads.push(p.thread_index(100 + i, || format!("http-nio-8080-exec-{i}")));
    }
    for (i, t) in p.threads.iter_mut().enumerate() {
        t.group = Some(if i == 0 { "main".into() } else { "http".into() });
        t.os_id = Some(5000 + i as u64);
        t.start = Some(start);
    }

    let leaf_stacks: Vec<(u64, ThreadState, StackId)> =
        LEAVES.iter().map(|(w, st, path)| (*w, *st, intern_path(&mut p, SERVLET, path))).collect();
    let idle = intern_path(
        &mut p,
        &SERVLET[..3],
        &[
            ("java.util.concurrent.LinkedBlockingQueue", "take", "()Ljava/lang/Object;"),
            ("jdk.internal.misc.Unsafe", "park", "(ZJ)V"),
        ],
    );
    let main_stack = intern_path(
        &mut p,
        &[],
        &[
            ("dev.ember.shop.ShopApplication", "main", "([Ljava/lang/String;)V"),
            ("java.lang.Thread", "join", "()V"),
            ("java.lang.Object", "wait", "(J)V"),
        ],
    );
    let alloc_stacks: Vec<(u64, StrId, StackId)> = ALLOC
        .iter()
        .map(|(w, cls, path)| {
            let c = p.symbols.intern_str(cls);
            (*w, c, intern_path(&mut p, SERVLET, path))
        })
        .collect();
    let total_w: u64 = leaf_stacks.iter().map(|l| l.0).sum();
    let alloc_total: u64 = alloc_stacks.iter().map(|a| a.0).sum();

    // CPU/wall samples: each worker thread is busy ~70% of the time, with a load spike at 20–35s.
    let mut cpu = SampleSet::default();
    let mut wall = SampleSet::default();
    let mut alloc = SampleSet::default();
    let mut monitor = SampleSet::default();
    let mut state_runs: Vec<(Nanos, ThreadState)> = vec![(start, ThreadState::Unknown); threads.len()];
    let mut t = start;
    while t < start + duration {
        let secs = (t - start) / NS;
        let busy = if (20..35).contains(&secs) { 0.95 } else { 0.65 };
        for (ti, &th) in threads.iter().enumerate() {
            let (state, stack) = if ti == 0 {
                (ThreadState::Waiting, main_stack)
            } else if rng.unit() < busy {
                let mut pick = rng.below(total_w);
                let mut chosen = leaf_stacks[0];
                for l in &leaf_stacks {
                    if pick < l.0 {
                        chosen = *l;
                        break;
                    }
                    pick -= l.0;
                }
                (chosen.1, chosen.2)
            } else {
                (ThreadState::Waiting, idle)
            };
            let jitter = rng.below(interval as u64 / 2) as i64;
            wall.push(t + jitter, th, state, stack, interval as u64, 1, None);
            if state == ThreadState::Runnable {
                cpu.push(t + jitter, th, state, stack, interval as u64, 1, None);
            }
            if state == ThreadState::Blocked {
                monitor.push(t + jitter, th, state, stack, interval as u64, 1, None);
            }
            // Thread state spans: close the previous run when the state changes.
            let run = &mut state_runs[ti];
            if run.1 != state {
                if run.1 != ThreadState::Unknown {
                    p.thread_states.push(th, run.0, t, run.1);
                }
                *run = (t, state);
            }
        }
        if rng.unit() < 0.6 {
            let mut pick = rng.below(alloc_total);
            for &(w, cls, stack) in &alloc_stacks {
                if pick < w {
                    let th = threads[1 + rng.below(8) as usize];
                    let bytes = 512 * 1024 + rng.below(256 * 1024);
                    alloc.push(t, th, ThreadState::Runnable, stack, bytes, 1, Some(cls));
                    break;
                }
                pick -= w;
            }
        }
        t += interval;
    }
    for (ti, &th) in threads.iter().enumerate() {
        let run = state_runs[ti];
        p.thread_states.push(th, run.0, start + duration, run.1);
    }
    p.samples.insert(SampleKind::Cpu, cpu);
    p.samples.insert(SampleKind::Wall, wall);
    p.samples.insert(SampleKind::Alloc, alloc);
    p.samples.insert(SampleKind::Monitor, monitor);

    // Telemetry at 1 Hz and a sawtooth heap with young GCs.
    let mut heap = 300.0 * 1024.0 * 1024.0;
    for s in 0..=60i64 {
        let ts = start + s * NS;
        let spike = (20..35).contains(&s);
        heap += (if spike { 90.0 } else { 45.0 }) * 1024.0 * 1024.0 * (0.8 + 0.4 * rng.unit());
        if heap > 1.4 * 1024.0 * 1024.0 * 1024.0 {
            let before = heap;
            heap = 280.0 * 1024.0 * 1024.0 + 40.0 * 1024.0 * 1024.0 * rng.unit();
            let pause = (6 + rng.below(20)) as i64 * 1_000_000;
            p.gc.push(GcEvent {
                start: ts,
                duration: pause,
                name: "G1 Young Generation".into(),
                cause: "G1 Evacuation Pause".into(),
                heap_before: Some(before as u64),
                heap_after: Some(heap as u64),
                pause: Some(pause),
            });
            p.telemetry_mut("gc.pause", "GC pause", Unit::Nanos).push(ts, pause as f64);
        }
        p.telemetry_mut("heap.used", "Heap used", Unit::Bytes).push(ts, heap);
        p.telemetry_mut("heap.committed", "Heap committed", Unit::Bytes)
            .push(ts, 1.5 * 1024.0 * 1024.0 * 1024.0);
        p.telemetry_mut("heap.max", "Heap max", Unit::Bytes).push(ts, 2.0 * 1024.0 * 1024.0 * 1024.0);
        let cpu = if spike { 0.85 } else { 0.45 } + 0.1 * rng.unit();
        p.telemetry_mut("cpu.process", "Process CPU", Unit::Percent).push(ts, cpu * 100.0);
        p.telemetry_mut("cpu.system", "System CPU", Unit::Percent).push(ts, (cpu + 0.08) * 100.0);
        p.telemetry_mut("threads.live", "Live threads", Unit::Count).push(ts, 42.0 + rng.below(3) as f64);
        p.telemetry_mut("threads.daemon", "Daemon threads", Unit::Count).push(ts, 30.0);
        p.telemetry_mut("classes.loaded", "Loaded classes", Unit::Count).push(ts, 14_200.0 + s as f64 * 3.0);
        p.telemetry_mut("alloc.rate", "Allocation rate", Unit::PerSecond)
            .push(ts, (if spike { 95.0 } else { 48.0 }) * 1024.0 * 1024.0);
    }

    // Monitor events on the product cache.
    let cache_stack = leaf_stacks[5].2;
    for i in 0..120 {
        let ts = start + (i * duration / 120);
        let th = threads[1 + (i as usize % 8)];
        let owner = threads[1 + ((i as usize + 3) % 8)];
        p.monitors.push(MonitorEvent {
            start: ts,
            duration: (1 + rng.below(40)) as i64 * 1_000_000,
            kind: MonitorEventKind::Enter,
            thread: th,
            owner: Some(owner),
            monitor_class: "dev.ember.shop.ProductCache".into(),
            monitor_id: Some(0x7f00_1000),
            stack: cache_stack,
        });
    }
    p.bookmarks.push(Bookmark { time: start + 20 * NS, label: "Load test started".into(), color: None });
    p.bookmarks.push(Bookmark { time: start + 35 * NS, label: "Load test finished".into(), color: None });

    let dump = |label: &str, time: Nanos, blocked: bool| ThreadDump {
        time,
        label: label.into(),
        threads: (1..=8)
            .map(|i| DumpedThread {
                name: format!("http-nio-8080-exec-{i}"),
                java_id: 100 + i,
                state: if blocked && i % 2 == 0 { "BLOCKED".into() } else { "RUNNABLE".into() },
                is_virtual: false,
                frames: vec![
                    "dev.ember.shop.ProductCache.get(ProductCache.java:57)".into(),
                    "dev.ember.shop.CatalogController.search(CatalogController.java:31)".into(),
                    "java.lang.Thread.run(Thread.java:1583)".into(),
                ],
                locked: vec![],
                waiting_on: (blocked && i % 2 == 0).then(|| "dev.ember.shop.ProductCache@7f001000".into()),
            })
            .collect(),
    };
    p.thread_dumps.push(dump("Before load", start + 10 * NS, false));
    p.thread_dumps.push(dump("During load", start + 25 * NS, true));
    p
}
