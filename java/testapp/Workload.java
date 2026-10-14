import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Test workload for Ember: a CPU-hot method, steady allocation, a contended monitor, a contended
 * ReentrantLock, sleeping threads and (JDK 21+) virtual threads contending on a lock. Usage:
 * java Workload.java [seconds] (0 = run until killed).
 */
public class Workload {
    static final Object LOCK = new Object();
    static final ReentrantLock JUC = new ReentrantLock();
    static final ReentrantLock VLOCK = new ReentrantLock();
    static volatile long sink;
    static List<byte[]> retained = new ArrayList<>();

    static long burnCpu(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc += (i * 31) ^ (acc >>> 3);
        }
        return acc;
    }

    static void allocate() {
        for (int i = 0; i < 1000; i++) {
            byte[] b = new byte[1024 + (i % 64)];
            sink += b.length;
            if (i % 100 == 0) {
                synchronized (retained) {
                    retained.add(b);
                    if (retained.size() > 5000) retained.clear();
                }
            }
        }
    }

    static void contend() throws InterruptedException {
        synchronized (LOCK) {
            Thread.sleep(5);
        }
    }

    static void contendJuc() throws InterruptedException {
        JUC.lock();
        try {
            Thread.sleep(5);
        } finally {
            JUC.unlock();
        }
    }

    static void virtualWork() throws InterruptedException {
        VLOCK.lock();
        try {
            sink += burnCpu(300_000);
            Thread.sleep(2);
        } finally {
            VLOCK.unlock();
        }
    }

    /** Starts virtual threads through reflection so this file still runs on JDK 11. */
    static void startVirtualThreads() {
        try {
            Object builder = Thread.class.getMethod("ofVirtual").invoke(null);
            Class<?> api = Class.forName("java.lang.Thread$Builder");
            for (int i = 0; i < 4; i++) {
                Object named = api.getMethod("name", String.class).invoke(builder, "vt-" + i);
                Runnable r = () -> {
                    try { while (true) virtualWork(); } catch (InterruptedException e) { }
                };
                api.getMethod("start", Runnable.class).invoke(named, r);
            }
        } catch (ReflectiveOperationException e) {
            // Pre-21: no virtual threads.
        }
    }

    public static void main(String[] args) throws Exception {
        long seconds = args.length > 0 ? Long.parseLong(args[0]) : 0;
        System.out.println("PID " + ProcessHandle.current().pid());
        System.out.flush();
        Thread burner = new Thread(() -> { while (true) sink += burnCpu(5_000_000); }, "burner");
        burner.setDaemon(true);
        burner.start();
        Thread allocator = new Thread(() -> { while (true) allocate(); }, "allocator");
        allocator.setDaemon(true);
        allocator.start();
        for (int i = 0; i < 2; i++) {
            Thread t = new Thread(() -> {
                try { while (true) contend(); } catch (InterruptedException e) { }
            }, "contender-" + i);
            t.setDaemon(true);
            t.start();
            Thread j = new Thread(() -> {
                try { while (true) contendJuc(); } catch (InterruptedException e) { }
            }, "juc-" + i);
            j.setDaemon(true);
            j.start();
        }
        startVirtualThreads();
        long end = System.currentTimeMillis() + seconds * 1000;
        while (seconds == 0 || System.currentTimeMillis() < end) {
            Thread.sleep(100);
        }
        System.out.println("DONE");
    }
}
