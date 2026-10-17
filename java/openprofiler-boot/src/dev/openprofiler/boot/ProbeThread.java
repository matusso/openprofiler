package dev.openprofiler.boot;

/** Per-thread probe state: the active calls, the request label and the re-entrancy guard. */
final class ProbeThread {
    private static final ThreadLocal<ProbeThread> CURRENT = new ThreadLocal<ProbeThread>() {
        @Override
        protected ProbeThread initialValue() {
            return new ProbeThread();
        }
    };

    /** A probe callback is running; hooks it reaches are ignored. */
    boolean busy;
    Call[] calls = new Call[8];
    int depth;
    /** The request this thread works for (propagated across hand-offs). */
    String context;
    /** {@link #context} was set by {@code ThreadPoolExecutor.beforeExecute}, to clear after it. */
    boolean pooledContext;

    static ProbeThread current() {
        try {
            return CURRENT.get();
        } catch (Throwable t) {
            return null;
        }
    }

    private final Call scratch = new Call();

    /** A call for entry-only interceptions, which are never on the stack. */
    Call scratch() {
        scratch.reset();
        return scratch;
    }

    Call push() {
        if (depth == calls.length) {
            Call[] n = new Call[depth * 2];
            System.arraycopy(calls, 0, n, 0, depth);
            calls = n;
        }
        Call c = calls[depth];
        if (c == null) {
            c = calls[depth] = new Call();
        } else {
            c.reset();
        }
        depth++;
        return c;
    }

    /** Whether a call of {@code probe} is active below the top. */
    boolean active(Probe probe) {
        for (int i = depth - 2; i >= 0; i--) {
            if (calls[i].probe == probe) {
                return true;
            }
        }
        return false;
    }

    void setContext(String label) {
        if (label == null ? context == null : label.equals(context)) {
            return;
        }
        context = label;
        Hooks.setContext0(label);
    }
}
