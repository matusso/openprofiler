import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.locks.*;

/** Test workload for ember-jfr: CPU-hot method, allocations, lock contention, parking, GC. */
public class Workload {
    static final Object LOCK = new Object();
    static volatile long sink;

    static long hotLoop(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (i * 31L) ^ (acc >>> 3);
            if ((i & 1023) == 0) acc = Long.rotateLeft(acc, 7);
        }
        return acc;
    }

    static byte[] allocate(int size) {
        return new byte[size];
    }

    static void contend() {
        synchronized (LOCK) {
            sink += hotLoop(200_000);
        }
    }

    public static void main(String[] args) throws Exception {
        long seconds = args.length > 0 ? Long.parseLong(args[0]) : 3;
        long end = System.nanoTime() + seconds * 1_000_000_000L;
        int threads = 4;
        ExecutorService pool = Executors.newFixedThreadPool(threads, r -> {
            Thread t = new Thread(r, "ember-worker");
            t.setDaemon(true);
            return t;
        });
        List<Future<?>> fs = new ArrayList<>();
        for (int t = 0; t < threads; t++) {
            final int id = t;
            fs.add(pool.submit(() -> {
                List<byte[]> keep = new ArrayList<>();
                while (System.nanoTime() < end) {
                    sink += hotLoop(2_000_000);
                    keep.add(allocate(64 * 1024));
                    if (keep.size() > 200) keep.clear();
                    if (id % 2 == 0) contend();
                    LockSupport.parkNanos(25_000_000L);
                }
            }));
        }
        while (System.nanoTime() < end) {
            Thread.sleep(500);
            System.gc();
        }
        for (Future<?> f : fs) f.get();
        pool.shutdown();
        System.out.println("done " + sink);
    }
}
