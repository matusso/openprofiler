package dev.openprofiler.boot;

import java.lang.management.ManagementFactory;
import java.lang.management.RuntimeMXBean;
import java.util.ArrayList;
import java.util.List;

/**
 * Entry point of the OpenProfiler boot classes. The native agent appends this jar to the boot class
 * path, binds the {@code native} methods below with {@code RegisterNatives} and calls
 * {@link #start()} once the VM is live.
 *
 * <p>Everything here must stay cheap and must never throw into application code.
 */
public final class Agent {
    private static volatile boolean started;

    private Agent() {}

    // ---- natives, bound by the agent ---------------------------------------------------------

    /** Declares the telemetry columns; called once before the first {@link #telemetry0}. */
    static native void telemetryKeys0(String[] keys, String[] labels, int[] units);

    /** One telemetry row, values in the order declared by {@link #telemetryKeys0}. */
    static native void telemetry0(long epochNanos, double[] values);

    static native void gcEvent0(long startNanos, long durationNanos, String name, String cause,
                                long heapBefore, long heapAfter, long pauseNanos);

    static native void bookmark0(String label, String color);

    static native void setRecording0(String kind, boolean enabled);

    static native String saveSnapshot0(String path);

    static native boolean registerPark0(Class<?> unsafeClass);

    // ---- called from native --------------------------------------------------------------------

    /** Starts the telemetry sampler. Idempotent. */
    public static void start() {
        if (started) {
            return;
        }
        started = true;
        Telemetry.startThread();
    }

    /** JVM description as alternating key/value strings. */
    public static String[] jvmInfo() {
        List<String> kv = new ArrayList<>();
        try {
            RuntimeMXBean rt = ManagementFactory.getRuntimeMXBean();
            put(kv, "pid", Long.toString(ProcessHandle.current().pid()));
            put(kv, "vm_name", System.getProperty("java.vm.name", ""));
            put(kv, "vm_version", System.getProperty("java.vm.version", ""));
            put(kv, "vm_vendor", System.getProperty("java.vm.vendor", ""));
            put(kv, "java_version", System.getProperty("java.version", ""));
            put(kv, "jvm_args", String.join("\u0001", rt.getInputArguments()));
            String cmd = System.getProperty("sun.java.command", "");
            int sp = cmd.indexOf(' ');
            put(kv, "main_class", sp < 0 ? cmd : cmd.substring(0, sp));
            put(kv, "name", rt.getName());
            put(kv, "os", System.getProperty("os.name", ""));
            put(kv, "arch", System.getProperty("os.arch", ""));
            put(kv, "cpus", Integer.toString(Runtime.getRuntime().availableProcessors()));
            put(kv, "max_heap", Long.toString(Runtime.getRuntime().maxMemory()));
            put(kv, "start_time", Long.toString(rt.getStartTime()));
        } catch (Throwable t) {
            // Partial info is better than none.
        }
        return kv.toArray(new String[0]);
    }

    private static void put(List<String> kv, String k, String v) {
        kv.add(k);
        kv.add(v == null ? "" : v);
    }

    /** Writes an HPROF heap dump; returns null on success or an error message. */
    public static String dumpHeap(String path, boolean live) {
        return Dumps.dumpHeap(path, live);
    }

    /**
     * Binds the agent's {@code Unsafe.park} wrapper. Done from here rather than from the agent
     * thread so the VM sees a boot-loader caller, which it accepts without a warning.
     */
    public static boolean registerPark(Class<?> unsafeClass) {
        return registerPark0(unsafeClass);
    }

    /** Thread dump in the line-oriented format documented in {@link Dumps#threadDump()}. */
    public static String[] threadDump() {
        return Dumps.threadDump();
    }

    // ---- MBean browser (line formats documented in MBeans) -------------------------------------

    public static String[] mbeanList(String pattern) {
        return MBeans.list(pattern);
    }

    public static String[] mbeanGet(String name) {
        return MBeans.get(name);
    }

    public static String[] mbeanInvoke(String name, String operation, String[] signature, String[] args) {
        return MBeans.invoke(name, operation, signature, args);
    }

    public static String[] mbeanSet(String name, String attribute, String value) {
        return MBeans.set(name, attribute, value);
    }

    public static String[] mbeanTrack(String name, String attribute, String item, String label, int unit,
                                      boolean enabled) {
        return MBeans.track(name, attribute, item, label, unit, enabled);
    }

    // ---- used by the Controller API ------------------------------------------------------------

    static void bookmark(String label, String color) {
        bookmark0(label, color);
    }

    static void setRecording(String kind, boolean enabled) {
        setRecording0(kind, enabled);
    }

    static String saveSnapshot(String path) {
        return saveSnapshot0(path);
    }
}
