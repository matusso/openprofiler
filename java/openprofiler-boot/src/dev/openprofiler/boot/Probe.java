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

    /**
     * Whether calls are timed even without a payload after {@link #onEnter} (one is set later, or
     * the time is used otherwise). Other calls without a payload are not timed: reading the clock
     * is a good part of a probe call's cost.
     */
    boolean alwaysTimed() {
        return false;
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
        int id = index >= 0 && payload != null ? Probes.payloadId(payload) : -1;
        if (id >= 0) {
            Hooks.probeCallLabelled0(index, id, startEpochNanos, durationNanos, error, 0, detail, Probes.labelId(label));
        }
    }

    /** Records a call that was not intercepted (custom probes, programmatic API). */
    protected final void record(String payload, long startEpochNanos, long durationNanos, String error,
                                String detail) {
        int id = index >= 0 && payload != null ? Probes.payloadId(payload) : -1;
        if (id >= 0) {
            Hooks.probeCall0(index, id, startEpochNanos, durationNanos, error, 0, detail);
        }
    }

    /**
     * Records {@code call} when {@code stage} (a {@code CompletionStage} the call returned)
     * completes, with the time from the call's start to completion and the request label the
     * calling thread had; {@code detail} turns the result into the event's detail. Returns false
     * (nothing recorded) if {@code stage} is not a {@code CompletionStage}.
     */
    protected final boolean recordOnCompletion(Call call, Object stage,
                                               java.util.function.Function<Object, String> detail) {
        if (!(stage instanceof java.util.concurrent.CompletionStage) || call.payload == null) {
            return false;
        }
        String payload = call.payload;
        long startEpochNanos = call.startEpochNanos;
        ProbeThread t = ProbeThread.current();
        String label = t == null ? null : t.context;
        ((java.util.concurrent.CompletionStage<?>) stage).whenComplete((result, error) -> {
            long duration = Probes.epochNanos() - startEpochNanos;
            String err = null;
            Throwable e = error instanceof java.util.concurrent.CompletionException && error.getCause() != null
                ? error.getCause() : error;
            if (e != null) {
                err = Reflect.cut(e.getClass().getName() + (e.getMessage() == null ? "" : ": " + e.getMessage()), 500);
            }
            String d = null;
            if (result != null && detail != null) {
                ProbeThread pt = ProbeThread.current();
                boolean busy = pt != null && pt.busy;
                if (pt != null) {
                    pt.busy = true;
                }
                try {
                    d = detail.apply(result);
                } catch (Throwable ignored) {
                    // no detail
                } finally {
                    if (pt != null) {
                        pt.busy = busy;
                    }
                }
            }
            recordFor(label, payload, startEpochNanos, duration, err, d);
        });
        call.ignore();
        return true;
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
