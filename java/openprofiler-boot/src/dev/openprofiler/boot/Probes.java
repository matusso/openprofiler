package dev.openprofiler.boot;

import java.util.concurrent.ConcurrentHashMap;

/**
 * The probe runtime: registers the built-in probes with the agent and dispatches the injected
 * {@link Hooks} calls to them.
 */
final class Probes {
    private static final long BASE_EPOCH = System.currentTimeMillis() * 1_000_000L;
    private static final long BASE_NANOS = System.nanoTime();

    /** Registration index → probe. */
    private static volatile Probe[] byIndex = new Probe[0];
    /** Site → {@code (probe << 32 | interception) + 1}; 0 = not looked up yet. */
    private static volatile long[] sites = new long[256];
    /**
     * Site → type the receiver must have (sites in superclasses that implement an intercepted
     * method for subtypes); written before {@link #sites}.
     */
    private static volatile String[] siteTypes = new String[256];
    private static final ConcurrentHashMap<String, CustomProbe> CUSTOM = new ConcurrentHashMap<>();
    private static volatile long failures;
    /** {@code -Dopenprofiler.probes.debug=true} prints the first probe failures. */
    private static final boolean DEBUG = Boolean.getBoolean("openprofiler.probes.debug");

    private Probes() {}

    static long epochNanos() {
        return BASE_EPOCH + (System.nanoTime() - BASE_NANOS);
    }

    /** Registers the built-in probes; called once from {@link Agent#start()}. */
    static void init() {
        Probe[] builtin = {
            new JdbcProbe(), new JpaProbe(), new HttpServerProbe(), new HttpClientProbe(), new KafkaProbe(),
            new GrpcProbe(), new ExceptionProbe(), new FileProbe(), new SocketProbe(), new AsyncProbe(),
        };
        try {
            for (Probe p : builtin) {
                register(p);
            }
            Hooks.ready = true;
            Hooks.ready0();
        } catch (UnsatisfiedLinkError e) {
            // Instrumentation unavailable in this JVM; probes stay inactive.
        }
    }

    static synchronized void register(Probe p) {
        p.index = Hooks.registerProbe0(p.id, p.name, p.payloadLabel, p.controlLabel == null ? "" : p.controlLabel,
                                       p.visible());
        Interception[] its = p.interceptions();
        p.flags = new int[its.length];
        for (int i = 0; i < its.length; i++) {
            p.flags[i] = its[i].flags;
            Interception it = its[i];
            Hooks.registerInterception0(p.index, i, it.type, it.method, it.descriptor, it.flags);
        }
        Probe[] cur = byIndex;
        if (p.index >= cur.length) {
            Probe[] n = new Probe[Math.max(p.index + 1, cur.length * 2)];
            System.arraycopy(cur, 0, n, 0, cur.length);
            cur = n;
        }
        cur[p.index] = p;
        byIndex = cur;
    }

    private static long siteInfo(int site) {
        long[] s = sites;
        if (site < s.length && s[site] != 0) {
            return s[site] - 1;
        }
        long info = Hooks.siteInfo0(site);
        if (info < 0) {
            return -1;
        }
        String type = Hooks.siteType0(site);
        synchronized (Probes.class) {
            String[] t = siteTypes;
            if (site >= t.length) {
                t = java.util.Arrays.copyOf(t, Math.max(site + 1, t.length * 2));
            }
            t[site] = type;
            siteTypes = t;
            s = sites;
            if (site >= s.length) {
                s = java.util.Arrays.copyOf(s, Math.max(site + 1, s.length * 2));
            }
            s[site] = info + 1;
            sites = s;
        }
        return info;
    }

    /** Whether {@code self} has the type the site requires (always true for most sites). */
    private static boolean receiverMatches(int site, Object self) {
        String[] t = siteTypes;
        String type = site < t.length ? t[site] : null;
        return type == null || Reflect.isA(self, type);
    }

    private static Probe probeFor(int site, long info) {
        int probe = (int) (info >>> 32);
        int interception = (int) info;
        if (interception == -1) {
            return CustomProbe.forSite(probe, site);
        }
        if (interception == ComplexityProbe.INTERCEPTION) {
            return ComplexityProbe.INSTANCE;
        }
        Probe[] ps = byIndex;
        return probe < ps.length ? ps[probe] : null;
    }

    static void enter(int site, Object self, Object[] args) {
        ProbeThread t = ProbeThread.current();
        if (t == null || t.busy) {
            return;
        }
        t.busy = true;
        try {
            long info = siteInfo(site);
            Probe p = info < 0 ? null : probeFor(site, info);
            if (p == null || !receiverMatches(site, self)) {
                return;
            }
            int interception = (int) info;
            if (interception >= 0 && interception < p.flags.length && (p.flags[interception] & Interception.ENTRY_ONLY) != 0) {
                // No exit follows: a scratch call that is not pushed.
                Call c = t.scratch();
                c.site = site;
                c.probe = p;
                c.interception = interception;
                c.self = self;
                p.onEnter(c, self, args);
                c.self = null;
                c.state = null;
                return;
            }
            Call c = t.push();
            c.site = site;
            c.probe = p;
            c.interception = (int) info;
            c.nested = t.active(p);
            c.self = self;
            c.startEpochNanos = epochNanos();
            p.onEnter(c, self, args);
            c.start = System.nanoTime();
        } catch (Throwable e) {
            failed(e);
        } finally {
            t.busy = false;
        }
    }

    private static void failed(Throwable e) {
        if (DEBUG && failures < 20) {
            System.err.println("[openprofiler] probe failure:");
            e.printStackTrace();
        }
        failures++;
    }

    static void exit(int site, Object result, Throwable error) {
        ProbeThread t = ProbeThread.current();
        if (t == null || t.busy) {
            return;
        }
        long end = System.nanoTime();
        t.busy = true;
        try {
            Call c = null;
            int match = t.depth - 1;
            while (match >= 0 && t.calls[match].site != site) {
                match--;
            }
            if (match >= 0) {
                // Calls above the match missed their exit (an exception before a constructor's
                // super call); they are dropped.
                while (t.depth > match + 1) {
                    Call top = t.calls[--t.depth];
                    if (top.setContext) {
                        t.setContext(top.previousContext);
                    }
                }
                c = t.calls[--t.depth];
            } else {
                // Exit-only interceptions (constructors) have no entry; other unmatched exits
                // belong to calls entered while hooks were inactive.
                long info = siteInfo(site);
                Probe p = info < 0 ? null : probeFor(site, info);
                if (p == null || !p.exitOnly((int) info)) {
                    return;
                }
                c = t.push();
                t.depth--;
                c.site = site;
                c.probe = p;
                c.interception = (int) info;
                c.nested = t.depth > 0 && activeAtOrBelow(t, p);
                c.startEpochNanos = epochNanos();
                c.start = end;
            }
            Probe p = c.probe;
            if (error != null) {
                c.error = Reflect.cut(error.getClass().getName() + (error.getMessage() == null ? "" : ": " + error.getMessage()), 500);
            }
            p.onExit(c, c.self != null ? c.self : result, result, error);
            if (c.setContext) {
                t.setContext(c.previousContext);
            }
            if (!c.nested && c.payload != null) {
                Hooks.probeCall0(p.index, c.payload, c.startEpochNanos, end - c.start, c.error, c.control, c.detail);
            }
            // Don't keep application objects reachable from the thread's call stack.
            c.self = null;
            c.state = null;
        } catch (Throwable e) {
            failed(e);
        } finally {
            t.busy = false;
        }
    }

    private static boolean activeAtOrBelow(ProbeThread t, Probe p) {
        for (int i = t.depth - 1; i >= 0; i--) {
            if (t.calls[i].probe == p) {
                return true;
            }
        }
        return false;
    }

    /** A custom probe by name, registered on first use. */
    static CustomProbe custom(String name) {
        CustomProbe p = CUSTOM.get(name);
        if (p != null) {
            return p;
        }
        synchronized (Probes.class) {
            p = CUSTOM.get(name);
            if (p == null) {
                p = new CustomProbe(name);
                try {
                    register(p);
                } catch (UnsatisfiedLinkError e) {
                    // no instrumentation; the probe records nothing
                }
                CUSTOM.put(name, p);
            }
        }
        return p;
    }

    static long failures() {
        return failures;
    }
}
