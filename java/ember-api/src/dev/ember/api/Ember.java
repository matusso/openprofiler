package dev.ember.api;

import java.lang.reflect.Method;
import java.nio.file.Path;

/**
 * Ember Controller API. Applications call these methods to control profiling programmatically.
 * When the Ember agent is not loaded, every method is a cheap no-op.
 */
public final class Ember {
    private static final Class<?> CONTROLLER = lookup();

    private Ember() {}

    private static Class<?> lookup() {
        try {
            // The agent's classes live on the boot class path (loader == null).
            return Class.forName("dev.ember.boot.Controller", true, null);
        } catch (Throwable t) {
            return null;
        }
    }

    /** Whether an Ember agent is loaded in this JVM. */
    public static boolean isProfiled() {
        return CONTROLLER != null;
    }

    private static Object call(String name, Class<?>[] types, Object... args) {
        if (CONTROLLER == null) {
            return null;
        }
        try {
            Method m = CONTROLLER.getMethod(name, types);
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
        if (CONTROLLER == null) {
            return "Ember agent not loaded";
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
}
