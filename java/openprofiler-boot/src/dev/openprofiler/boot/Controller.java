package dev.openprofiler.boot;

/**
 * Boot-side implementation of the public Controller API ({@code dev.openprofiler.api.OpenProfiler}). The API
 * jar finds this class reflectively through the boot class loader; without an agent the API calls
 * are no-ops.
 */
public final class Controller {
    private Controller() {}

    public static void startCpuRecording() {
        Agent.setRecording("cpu", true);
    }

    public static void stopCpuRecording() {
        Agent.setRecording("cpu", false);
    }

    public static void startAllocRecording() {
        Agent.setRecording("alloc", true);
    }

    public static void stopAllocRecording() {
        Agent.setRecording("alloc", false);
    }

    public static void startMonitorRecording() {
        Agent.setRecording("monitor", true);
    }

    public static void stopMonitorRecording() {
        Agent.setRecording("monitor", false);
    }

    /** Returns null on success or an error message. */
    public static String saveSnapshot(String path) {
        return Agent.saveSnapshot(path);
    }

    public static void bookmark(String label, String color) {
        Agent.bookmark(label, color);
    }
}
