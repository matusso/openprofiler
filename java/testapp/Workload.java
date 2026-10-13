import java.util.ArrayList;
import java.util.List;

/**
 * Test workload for Ember: a CPU-hot method, steady allocation, a contended monitor and sleeping
 * threads. Usage: java Workload.java [seconds] (0 = run until killed).
 */
public class Workload {
    static final Object LOCK = new Object();
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
        }
        long end = System.currentTimeMillis() + seconds * 1000;
        while (seconds == 0 || System.currentTimeMillis() < end) {
            Thread.sleep(100);
        }
        System.out.println("DONE");
    }
}
