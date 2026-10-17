package dev.openprofiler.api;

import java.lang.reflect.Method;
import java.nio.file.Path;

/**
 * OpenProfiler Controller API. Applications call these methods to control profiling programmatically.
 * When the OpenProfiler agent is not loaded, every method is a cheap no-op.
 */
public final class OpenProfiler {
    private static volatile Class<?> controller = lookup();
    /** When the agent was last looked for (it may be attached later). */
    private static volatile long lastLookup = System.nanoTime();

    private OpenProfiler() {}

    private static Class<?> lookup() {
        try {
            // The agent's classes live on the boot class path (loader == null).
            return Class.forName("dev.openprofiler.boot.Controller", true, null);
        } catch (Throwable t) {
            return null;
        }
    }

    /** The agent's controller class; looked up again at most once a second while absent. */
    private static Class<?> controller() {
        Class<?> c = controller;
        if (c == null && System.nanoTime() - lastLookup > 1_000_000_000L) {
            lastLookup = System.nanoTime();
            c = controller = lookup();
        }
        return c;
    }

    /** Whether an OpenProfiler agent is loaded in this JVM. */
    public static boolean isProfiled() {
        return controller() != null;
    }

    private static Object call(String name, Class<?>[] types, Object... args) {
        Class<?> c = controller();
        if (c == null) {
            return null;
        }
        try {
            Method m = c.getMethod(name, types);
            return m.invoke(null, args);
        } catch (Throwable t) {
            return null;
        }
    }

    public static void startCpuRecording() {
        call("startCpuRecording", new Class<?>[0]);
    }

    public static void stopCpuRecording() {
        call("stopCpuRecording", new Class<?>[0]);
    }

    public static void startAllocRecording() {
        call("startAllocRecording", new Class<?>[0]);
    }

    public static void stopAllocRecording() {
        call("stopAllocRecording", new Class<?>[0]);
    }

    public static void startMonitorRecording() {
        call("startMonitorRecording", new Class<?>[0]);
    }

    public static void stopMonitorRecording() {
        call("stopMonitorRecording", new Class<?>[0]);
    }

    /**
     * Saves the data recorded so far to {@code path}.
     *
     * @return {@code null} on success, otherwise an error message (also when no agent is loaded)
     */
    public static String saveSnapshot(Path path) {
        if (controller() == null) {
            return "OpenProfiler agent not loaded";
        }
        Object r = call("saveSnapshot", new Class<?>[] {String.class}, path.toAbsolutePath().toString());
        return r == null ? null : r.toString();
    }

    /** Adds a bookmark to the telemetry timeline. */
    public static void bookmark(String label) {
        bookmark(label, null);
    }

    /** Adds a coloured bookmark (CSS colour, e.g. {@code "#e5534b"}). */
    public static void bookmark(String label, String color) {
        call("bookmark", new Class<?>[] {String.class, String.class}, label, color);
    }

    /**
     * Records one call of custom probe {@code probe} (shown as {@code custom.<probe>}): what it did
     * ({@code payload}) and how long it took. For methods, {@link Probe @Probe} does this
     * automatically.
     */
    public static void probeEvent(String probe, String payload, long durationNanos) {
        probeEvent(probe, payload, durationNanos, null);
    }

    /** Like {@link #probeEvent(String, String, long)}, for a call that failed with {@code error}. */
    public static void probeEvent(String probe, String payload, long durationNanos, String error) {
        call("probeEvent", new Class<?>[] {String.class, String.class, long.class, String.class},
             probe, payload, durationNanos, error);
    }
}
