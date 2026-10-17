package dev.openprofiler.boot;

/**
 * Entry points called by instrumented code. The agent's class-file rewriter injects calls to these
 * methods; the natives are bound by the agent before any instrumented code can run.
 *
 * <p>Call counting goes straight to native code ({@link #callEnter}/{@link #callExit}); probe hooks
 * dispatch to the {@link Probe}s through {@link Probes}. Nothing here may throw into application
 * code.
 */
public final class Hooks {
    /** Set once the probes are registered; hooks reached earlier (or during class init) do nothing. */
    static volatile boolean ready;

    private Hooks() {}

    // ---- natives, bound by the agent ---------------------------------------------------------

    /** Instrumented call counting: method entry. */
    public static native void callEnter(int method);

    /** Instrumented call counting: method exit (normal or exceptional). */
    public static native void callExit(int method);

    /** A method trigger's method was called. */
    public static native void trigger(int site);

    static native int registerProbe0(String id, String name, String payloadLabel, String controlLabel,
                                     boolean visible);

    static native void registerInterception0(int probe, int index, String type, String method, String desc,
                                             int flags);

    /** All probes are registered: instruments loaded classes and starts the instrumentation thread. */
    static native void ready0();

    /** {@code probe << 32 | interception} of an instrumented site ({@code 0xFFFFFFFF} for custom probes). */
    static native long siteInfo0(int site);

    /** Payload template of a custom probe site. */
    static native String siteTemplate0(int site);

    static native void probeCall0(int probe, String payload, long startEpochNanos, long durationNanos,
                                  String error, long control, String detail);

    static native void control0(int probe, long id, boolean open, String description);

    /** Sets (null: clears) the current thread's request label. */
    static native void setContext0(String label);

    // ---- injected calls ------------------------------------------------------------------------

    /** Probe method entry; {@code args} are the boxed arguments if the interception asked for them. */
    public static void enter(int site, Object self, Object[] args) {
        if (ready) {
            Probes.enter(site, self, args);
        }
    }

    /** Probe method exit; {@code result} is the boxed return value ({@code this} for constructors). */
    public static void exit(Object result, int site) {
        if (ready) {
            Probes.exit(site, result, null);
        }
    }

    /** Probe method exit by an exception, which is rethrown afterwards. */
    public static void exitThrow(Throwable error, int site) {
        if (ready) {
            Probes.exit(site, null, error);
        }
    }
}
