package dev.openprofiler.boot;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.time.Duration;
import java.time.Instant;
import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.function.Consumer;
import jdk.jfr.EventType;
import jdk.jfr.FlightRecorder;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedFrame;
import jdk.jfr.consumer.RecordedMethod;
import jdk.jfr.consumer.RecordedStackTrace;
import jdk.jfr.consumer.RecordedThread;

/**
 * CPU sampler back end that streams JFR execution samples to the agent (sampler=jfr).
 *
 * <p>{@code RecordingStream} (JDK 14+) is used reflectively because the boot classes compile for
 * Java 11; the event and frame types exist there. One thread, {@code OpenProfiler JFR}, runs the
 * stream; the agent's thread filter skips it by name. When the sampling period changes or CPU
 * recording is switched off the stream is closed at its next flush and reopened as needed.
 */
final class JfrSampler implements Runnable {
    static final String EXECUTION = "jdk.ExecutionSample";
    static final String CPU_TIME = "jdk.CPUTimeSample";
    /** Identity caches are also cleared at every flush (JFR creates new objects per chunk). */
    private static final int CACHE_LIMIT = 10_000;

    // ---- natives, bound by the agent ---------------------------------------------------------

    /** Sampling period in nanoseconds; 0 while CPU recording is off, -1 once the agent stops. */
    static native long period0();

    /** Interns a method and returns the agent's id for it. */
    static native int method0(String className, String name, String descriptor);

    static native void sample0(long javaThreadId, long osThreadId, long epochNanos, long weight, int depth,
                               int[] methods, int[] lines, byte[] kinds);

    /** The stream ended for good; the agent falls back to its sync sampler. */
    static native void stopped0(String why);

    // ---- stream --------------------------------------------------------------------------------

    private static final class Stack {
        final int[] methods;
        final int[] lines;
        final byte[] kinds;

        Stack(int depth) {
            methods = new int[depth];
            lines = new int[depth];
            kinds = new byte[depth];
        }
    }

    private static final class MethodKey {
        final String type;
        final String name;
        final String descriptor;

        MethodKey(RecordedMethod m) {
            type = m.getType() == null ? "" : m.getType().getName();
            name = m.getName();
            descriptor = m.getDescriptor();
        }

        @Override
        public boolean equals(Object o) {
            if (!(o instanceof MethodKey)) {
                return false;
            }
            MethodKey k = (MethodKey) o;
            return type.equals(k.type) && Objects.equals(name, k.name) && Objects.equals(descriptor, k.descriptor);
        }

        @Override
        public int hashCode() {
            return (type.hashCode() * 31 + Objects.hashCode(name)) * 31 + Objects.hashCode(descriptor);
        }
    }

    private final String event;
    private final Class<?> streamClass;
    private final Map<MethodKey, Integer> methodIds = new HashMap<>();
    private final Map<RecordedMethod, Integer> methodCache = new IdentityHashMap<>();
    private final Map<RecordedStackTrace, Stack> stackCache = new IdentityHashMap<>();
    private Object stream;
    /** Set when this side closed the stream (period change); otherwise its end was unexpected. */
    private boolean closing;
    private static volatile boolean exiting;

    private JfrSampler(String event, Class<?> streamClass) {
        this.event = event;
        this.streamClass = streamClass;
    }

    /**
     * Opens the stream and starts the sampler thread; returns the streamed event's name. Throws if
     * JFR streaming is unavailable (JDK 11-13, JFR disabled, {@code jdk.jfr} not in the image).
     *
     * @param choice 0 auto, 1 {@code jdk.ExecutionSample}, 2 {@code jdk.CPUTimeSample}
     */
    static String start(int choice) throws Exception {
        Class<?> streamClass;
        try {
            streamClass = Class.forName("jdk.jfr.consumer.RecordingStream");
        } catch (ClassNotFoundException e) {
            throw new UnsupportedOperationException("JFR streaming needs JDK 14 or newer");
        }
        if (!FlightRecorder.isAvailable()) {
            throw new UnsupportedOperationException("JFR is not available in this JVM");
        }
        boolean cpuTime = hasEvent(CPU_TIME);
        if (choice == 2 && !cpuTime) {
            throw new UnsupportedOperationException(CPU_TIME + " needs JDK 25 or newer on Linux");
        }
        String event = choice == 2 || (choice == 0 && cpuTime) ? CPU_TIME : EXECUTION;
        JfrSampler s = new JfrSampler(event, streamClass);
        long p = period0();
        if (p > 0) {
            // Open now so the agent learns whether streaming works before relying on it.
            s.stream = s.open(p);
        }
        Runtime.getRuntime().addShutdownHook(new Thread(() -> exiting = true, "OpenProfiler JFR exit"));
        Thread t = new Thread(s, "OpenProfiler JFR");
        t.setDaemon(true);
        t.start();
        return event;
    }

    private static boolean hasEvent(String name) {
        // The CPU-time sampler is built on Linux thread CPU timers; other platforms only have the type.
        if (!System.getProperty("os.name", "").startsWith("Linux")) {
            return false;
        }
        for (EventType t : FlightRecorder.getFlightRecorder().getEventTypes()) {
            if (t.getName().equals(name)) {
                return true;
            }
        }
        return false;
    }

    @Override
    public void run() {
        try {
            while (true) {
                if (stream == null) {
                    long p = period0();
                    if (p < 0) {
                        return;
                    }
                    if (p == 0) {
                        Thread.sleep(200);
                        continue;
                    }
                    stream = open(p);
                }
                closing = false;
                call(stream, "start");
                stream = null;
                methodCache.clear();
                stackCache.clear();
                if (!closing) {
                    // JFR closed it: the VM is exiting (its shutdown hook stops all recordings) or
                    // someone stopped JFR. Give a shutdown a moment to show before reopening.
                    Thread.sleep(100);
                }
                if (exiting || period0() < 0) {
                    return;
                }
            }
        } catch (Throwable t) {
            if (!exiting && period0() >= 0) {
                stopped0(t.toString());
            }
        }
    }

    private Object open(long periodNanos) throws Exception {
        Object s = streamClass.getConstructor().newInstance();
        try {
            // JFR samples at most every millisecond.
            long effective = Math.max(1_000_000, periodNanos);
            Object settings = call(s, "enable", event);
            if (event.equals(CPU_TIME)) {
                // One sample per this much CPU time of a thread.
                call(settings, "with", "throttle", (effective / 1_000_000) + "ms");
            } else {
                call(settings, "withPeriod", Duration.ofNanos(effective));
            }
            call(s, "setReuse", true);
            call(s, "setOrdered", false);
            Consumer<RecordedEvent> onEvent = e -> onEvent(e, effective);
            call(s, "onEvent", event, onEvent);
            Object self = s;
            Runnable onFlush = () -> {
                methodCache.clear();
                stackCache.clear();
                if (period0() != periodNanos) {
                    closing = true;
                    try {
                        call(self, "close");
                    } catch (Exception ignored) {
                        // Closing a closed stream.
                    }
                }
            };
            call(s, "onFlush", onFlush);
            return s;
        } catch (Exception e) {
            call(s, "close");
            throw e;
        }
    }

    private void onEvent(RecordedEvent e, long periodNanos) {
        try {
            if (e.hasField("failed") && e.getBoolean("failed")) {
                return;
            }
            RecordedThread t = e.hasField("sampledThread") ? e.getThread("sampledThread") : e.getThread();
            RecordedStackTrace st = e.getStackTrace();
            if (t == null || st == null) {
                return;
            }
            long weight = periodNanos;
            if (e.hasField("samplingPeriod")) {
                Duration d = e.getDuration("samplingPeriod");
                if (d != null && !d.isZero() && !d.isNegative()) {
                    weight = d.toNanos();
                }
            }
            Stack s = stack(st);
            Instant time = e.getStartTime();
            sample0(t.getJavaThreadId(), t.getOSThreadId(), time.getEpochSecond() * 1_000_000_000L + time.getNano(),
                    weight, s.methods.length, s.methods, s.lines, s.kinds);
        } catch (Throwable ignored) {
            // A malformed event must not end the stream.
        }
    }

    private Stack stack(RecordedStackTrace st) {
        Stack s = stackCache.get(st);
        if (s != null) {
            return s;
        }
        List<RecordedFrame> frames = st.getFrames();
        int depth = 0;
        for (RecordedFrame f : frames) {
            if (f.isJavaFrame() && f.getMethod() != null) {
                depth++;
            }
        }
        s = new Stack(depth);
        int i = 0;
        for (RecordedFrame f : frames) {
            RecordedMethod m = f.getMethod();
            if (!f.isJavaFrame() || m == null) {
                continue;
            }
            s.methods[i] = methodId(m);
            s.lines[i] = f.getLineNumber();
            s.kinds[i] = kind(f.getType());
            i++;
        }
        if (stackCache.size() >= CACHE_LIMIT) {
            stackCache.clear();
        }
        stackCache.put(st, s);
        return s;
    }

    private int methodId(RecordedMethod m) {
        Integer id = methodCache.get(m);
        if (id == null) {
            MethodKey k = new MethodKey(m);
            id = methodIds.get(k);
            if (id == null) {
                id = method0(k.type, k.name, k.descriptor);
                methodIds.put(k, id);
            }
            if (methodCache.size() >= CACHE_LIMIT) {
                methodCache.clear();
            }
            methodCache.put(m, id);
        }
        return id;
    }

    /** JFR frame type to the agent's frame kind (0 interpreted, 1 JIT, 2 inlined, 3 native, 6 unknown). */
    static byte kind(String type) {
        if (type == null) {
            return 6;
        }
        switch (type) {
            case "Interpreted":
                return 0;
            case "JIT compiled":
                return 1;
            case "Inlined":
                return 2;
            case "Native":
                return 3;
            default:
                return 6;
        }
    }

    /** Calls a public method by name; arguments must match its parameter types exactly or by boxing. */
    private static Object call(Object target, String name, Object... args) throws Exception {
        // Look methods up on the public API type: JFR returns private implementation subclasses.
        Class<?> api = target.getClass();
        while (!Modifier.isPublic(api.getModifiers())) {
            api = api.getSuperclass();
        }
        for (Method m : api.getMethods()) {
            if (m.getName().equals(name) && m.getParameterCount() == args.length && accepts(m.getParameterTypes(), args)) {
                try {
                    return m.invoke(target, args);
                } catch (InvocationTargetException e) {
                    Throwable c = e.getCause();
                    if (c instanceof Exception) {
                        throw (Exception) c;
                    }
                    throw e;
                }
            }
        }
        throw new NoSuchMethodException(target.getClass().getName() + "." + name);
    }

    private static boolean accepts(Class<?>[] types, Object[] args) {
        for (int i = 0; i < types.length; i++) {
            Class<?> t = types[i];
            if (t == boolean.class) {
                t = Boolean.class;
            }
            if (!t.isInstance(args[i])) {
                return false;
            }
        }
        return true;
    }
}
