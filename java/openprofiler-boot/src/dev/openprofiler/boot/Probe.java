package dev.openprofiler.boot;

/**
 * The probe SPI. A probe names the methods it intercepts and turns their calls into payloads
 * (recorded as probe calls and events), control objects (things opened and closed, such as
 * connections) and request labels (which split call trees and follow work across threads).
 *
 * <p>Callbacks run on the application thread inside the intercepted method and must be cheap and
 * never throw; failures are caught and counted. Calls made by a probe callback are not intercepted.
 */
public abstract class Probe {
    final String id;
    final String name;
    final String payloadLabel;
    final String controlLabel;
    /** Registration index in the agent, -1 until registered. */
    int index = -1;
    /** {@link Interception} flags by interception index. */
    int[] flags = new int[0];

    private final WeakIdentityMap<Object, Control> controls = new WeakIdentityMap<>();
    private static final java.util.concurrent.atomic.AtomicLong NEXT_CONTROL = new java.util.concurrent.atomic.AtomicLong();

    protected Probe(String id, String name, String payloadLabel, String controlLabel) {
        this.id = id;
        this.name = name;
        this.payloadLabel = payloadLabel;
        this.controlLabel = controlLabel;
    }

    /** The methods this probe intercepts; their index is {@link Call#interception}. */
    protected abstract Interception[] interceptions();

    /** Method entry. */
    protected void onEnter(Call call, Object self, Object[] args) {}

    /** Method exit; {@code result} is the return value ({@code this} for constructors). */
    protected void onExit(Call call, Object self, Object result, Throwable error) {}

    boolean exitOnly(int interception) {
        return interception >= 0 && interception < flags.length && (flags[interception] & Interception.EXIT_ONLY) != 0;
    }

    /** Probes that only propagate context are not shown. */
    boolean visible() {
        return true;
    }

    // ---- emitters ------------------------------------------------------------------------------

    /** A control object was opened. Returns its id; opening an object twice keeps the first id. */
    protected final long openControl(Object object, String description) {
        if (object == null) {
            return 0;
        }
        expireControls();
        Control c = controls.get(object);
        if (c != null) {
            return c.id;
        }
        c = new Control(NEXT_CONTROL.incrementAndGet(), description);
        controls.put(object, c);
        Hooks.control0(index, c.id, true, description);
        return c.id;
    }

    /** A control object was closed. */
    protected final void closeControl(Object object) {
        Control c = object == null ? null : controls.remove(object);
        if (c != null) {
            Hooks.control0(index, c.id, false, null);
        }
    }

    /** Id of an open control object, 0 if unknown. */
    protected final long controlId(Object object) {
        Control c = object == null ? null : controls.get(object);
        return c == null ? 0 : c.id;
    }

    /** Description of an open control object (e.g. a file's path), or null. */
    protected final String controlDescription(Object object) {
        Control c = object == null ? null : controls.get(object);
        return c == null ? null : c.description;
    }

    /** Control objects collected without being closed are reported as closed by the GC. */
    private void expireControls() {
        for (Control c : controls.expunge()) {
            Hooks.control0(index, c.id, false, "garbage collected without close");
        }
    }

    /**
     * Records a call of request {@code label} that completed on another thread than it started
     * (asynchronous servers): the label is its context and the root of its stack.
     */
    protected final void recordFor(String label, String payload, long startEpochNanos, long durationNanos,
                                   String error, String detail) {
        if (index >= 0 && payload != null) {
            Hooks.probeCallLabelled0(index, payload, startEpochNanos, durationNanos, error, 0, detail, label);
        }
    }

    /** Records a call that was not intercepted (custom probes, programmatic API). */
    protected final void record(String payload, long startEpochNanos, long durationNanos, String error,
                                String detail) {
        if (index >= 0 && payload != null) {
            Hooks.probeCall0(index, payload, startEpochNanos, durationNanos, error, 0, detail);
        }
    }

    // ---- helpers for subclasses ----------------------------------------------------------------

    /** Sets the current thread's request label until the call exits. */
    protected static void label(Call call, String label) {
        ProbeThread t = ProbeThread.current();
        if (t == null) {
            return;
        }
        call.previousContext = t.context;
        call.setContext = true;
        t.setContext(label);
    }

    static final class Control {
        final long id;
        final String description;

        Control(long id, String description) {
            this.id = id;
            this.description = description;
        }
    }
}
