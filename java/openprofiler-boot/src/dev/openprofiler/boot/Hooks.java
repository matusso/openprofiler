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

    /** Type the receiver of a site must have (inherited implementations), or null. */
    static native String siteType0(int site);

    /** Payload template of a custom probe site. */
    static native String siteTemplate0(int site);

    /** The agent's id of a payload ({@link #probeCall0}), -1 if the agent is not recording. */
    static native int payloadId0(String payload);

    /** The agent's frame of a request label ({@link #setContext0}), -1 if unavailable. */
    static native int labelId0(String label);

    /** A probe call; {@code payload} is a {@link #payloadId0} id. */
    static native void probeCall0(int probe, int payload, long startEpochNanos, long durationNanos,
                                  String error, long control, String detail);

    /** {@link #probeCall0} for a call of request {@code label} (a {@link #labelId0} frame) recorded on another thread. */
    static native void probeCallLabelled0(int probe, int payload, long startEpochNanos, long durationNanos,
                                          String error, long control, String detail, int label);

    static native void control0(int probe, long id, boolean open, String description);

    /** Sets (-1: clears) the current thread's request label, a {@link #labelId0} frame. */
    static native void setContext0(int label);

    /** A call of a complexity-analysis site took {@code nanos} for an argument of size {@code size}. */
    static native void complexity0(int site, long size, long nanos);

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
