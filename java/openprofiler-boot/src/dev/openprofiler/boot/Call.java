package dev.openprofiler.boot;

/**
 * One intercepted call, passed to {@link Probe#onEnter} and {@link Probe#onExit}. A call with a
 * payload is recorded when it exits (aggregated into the probe's hot spots, and kept as an event
 * within the event budget), unless it is nested in another call of the same probe or ignored.
 */
public final class Call {
    /** Index of the interception in {@link Probe#interceptions()}. */
    public int interception;
    /** Free for the probe, e.g. state computed at entry. */
    public Object state;
    /** What the call did: an SQL statement, a URL. Null: not recorded. */
    public String payload;
    /** Extra information for the event (bind parameters, status, row count). */
    public String detail;
    /** Error text; defaults to the exception the call threw. */
    public String error;
    /** Control object id the call belongs to, 0 if none. */
    public long control;
    /** Another call of the same probe is active on this thread (outer calls are recorded). */
    public boolean nested;
    /** Wall-clock start, epoch nanoseconds. */
    public long startEpochNanos;

    int site;
    Probe probe;
    Object self;
    long start;
    /** {@link #start} and {@link #startEpochNanos} were read. */
    boolean timed;
    /** Request label to restore at exit (probes that set one). */
    String previousContext;
    boolean setContext;

    void reset() {
        interception = 0;
        state = null;
        payload = null;
        detail = null;
        error = null;
        control = 0;
        nested = false;
        previousContext = null;
        setContext = false;
        timed = false;
        probe = null;
        self = null;
    }

    /** Don't record this call. */
    public void ignore() {
        payload = null;
    }
}
