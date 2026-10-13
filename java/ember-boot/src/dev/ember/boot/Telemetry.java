package dev.ember.boot;

import java.lang.management.ClassLoadingMXBean;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryMXBean;
import java.lang.management.MemoryUsage;
import java.lang.management.OperatingSystemMXBean;
import java.lang.management.ThreadMXBean;
import java.lang.reflect.Method;
import java.util.List;

/** Once-per-second MXBean sampler that pushes rows into the native telemetry ring. */
final class Telemetry implements Runnable {
    // Units: 0 nanos, 1 bytes, 2 count, 3 percent, 4 per second.
    private static final String[] KEYS = {
        "heap.used", "heap.committed", "heap.max", "nonheap.used", "gc.activity", "gc.count",
        "threads.live", "threads.daemon", "classes.loaded", "cpu.process", "cpu.system", "alloc.rate",
    };
    private static final String[] LABELS = {
        "Used heap", "Committed heap", "Max heap", "Used non-heap", "GC activity", "GC count",
        "Live threads", "Daemon threads", "Loaded classes", "Process CPU", "System CPU", "Allocation rate",
    };
    private static final int[] UNITS = {1, 1, 1, 1, 3, 2, 2, 2, 2, 3, 3, 4};

    private final MemoryMXBean memory = ManagementFactory.getMemoryMXBean();
    private final List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
    private final ThreadMXBean threads = ManagementFactory.getThreadMXBean();
    private final ClassLoadingMXBean classes = ManagementFactory.getClassLoadingMXBean();
    private final OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
    private Method processCpu;
    private Method systemCpu;
    private Method totalAllocated;

    private long lastGcTime = -1;
    private long lastWall;
    private long lastAllocated = -1;

    static void startThread() {
        Thread t = new Thread(new Telemetry(), "Ember Telemetry");
        t.setDaemon(true);
        t.start();
        GcListener.install();
    }

    private Telemetry() {
        processCpu = method(os, "getProcessCpuLoad");
        systemCpu = method(os, "getCpuLoad");
        if (systemCpu == null) {
            systemCpu = method(os, "getSystemCpuLoad");
        }
        totalAllocated = method(threads, "getTotalThreadAllocatedBytes");
    }

    private static Method method(Object target, String name) {
        try {
            for (Class<?> c : target.getClass().getInterfaces()) {
                if (c.getName().startsWith("com.sun.management.")) {
                    return c.getMethod(name);
                }
            }
            Method m = target.getClass().getMethod(name);
            m.setAccessible(true);
            return m;
        } catch (Throwable t) {
            return null;
        }
    }

    private static double call(Method m, Object target) {
        if (m == null) {
            return Double.NaN;
        }
        try {
            Object v = m.invoke(target);
            return v instanceof Number ? ((Number) v).doubleValue() : Double.NaN;
        } catch (Throwable t) {
            return Double.NaN;
        }
    }

    @Override
    public void run() {
        try {
            Agent.telemetryKeys0(KEYS, LABELS, UNITS);
        } catch (Throwable t) {
            return;
        }
        while (true) {
            try {
                sample();
                Thread.sleep(1000);
            } catch (InterruptedException e) {
                return;
            } catch (Throwable t) {
                // Keep sampling; a failing MXBean must not kill telemetry.
                try {
                    Thread.sleep(1000);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }
    }

    private void sample() {
        long now = System.currentTimeMillis() * 1_000_000L;
        long wall = System.nanoTime();
        MemoryUsage heap = memory.getHeapMemoryUsage();
        MemoryUsage nonHeap = memory.getNonHeapMemoryUsage();
        long gcTime = 0;
        long gcCount = 0;
        for (GarbageCollectorMXBean gc : gcs) {
            gcTime += Math.max(0, gc.getCollectionTime());
            gcCount += Math.max(0, gc.getCollectionCount());
        }
        double gcActivity = 0;
        if (lastGcTime >= 0 && wall > lastWall) {
            gcActivity = Math.min(100.0, (gcTime - lastGcTime) * 1e6 * 100.0 / (wall - lastWall));
        }
        double allocRate = Double.NaN;
        double allocated = call(totalAllocated, threads);
        if (!Double.isNaN(allocated)) {
            if (lastAllocated >= 0 && wall > lastWall) {
                allocRate = (allocated - lastAllocated) * 1e9 / (wall - lastWall);
            }
            lastAllocated = (long) allocated;
        }
        lastGcTime = gcTime;
        lastWall = wall;
        double pcpu = call(processCpu, os);
        double scpu = call(systemCpu, os);
        double[] row = {
            heap.getUsed(), heap.getCommitted(), heap.getMax(), nonHeap.getUsed(), gcActivity, gcCount,
            threads.getThreadCount(), threads.getDaemonThreadCount(), classes.getLoadedClassCount(),
            pcpu >= 0 ? pcpu * 100.0 : Double.NaN, scpu >= 0 ? scpu * 100.0 : Double.NaN, allocRate,
        };
        Agent.telemetry0(now, row);
    }
}
