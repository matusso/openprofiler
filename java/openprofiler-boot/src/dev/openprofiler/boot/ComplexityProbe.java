package dev.openprofiler.boot;

import java.lang.reflect.Array;
import java.util.Collection;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Complexity analysis: methods named by the agent's {@code complexity=} specs report the size of
 * one argument and their duration, so time can be plotted against input size. The size is a
 * collection's or map's size, an array's or string's length, or a number's value. The site's
 * argument spec (from {@link Hooks#siteTemplate0}) is a parameter index, {@code this}, or empty
 * for the first argument that has a size. Calls record nothing else (no payload).
 */
final class ComplexityProbe extends Probe {
    /** Interception index the agent reports for complexity sites. */
    static final int INTERCEPTION = -2;
    static final ComplexityProbe INSTANCE = new ComplexityProbe();

    /** Site → argument index; -1 = first sized argument, -2 = the receiver. */
    private static final ConcurrentHashMap<Integer, Integer> ARGS = new ConcurrentHashMap<>();

    private ComplexityProbe() {
        super("complexity", "Complexity", "Method", null);
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[0];
    }

    @Override
    boolean visible() {
        return false;
    }

    @Override
    boolean alwaysTimed() {
        return true;
    }

    private static int argIndex(int site) {
        Integer i = ARGS.get(site);
        if (i == null) {
            String spec = Hooks.siteTemplate0(site);
            int v = -1;
            if (spec != null && spec.equals("this")) {
                v = -2;
            } else if (spec != null && !spec.isEmpty()) {
                try {
                    v = Integer.parseInt(spec.trim());
                } catch (NumberFormatException e) {
                    v = -1;
                }
            }
            i = v;
            ARGS.put(site, i);
        }
        return i;
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        int i = argIndex(call.site);
        long size = -1;
        if (i == -2) {
            size = size(self);
        } else if (i >= 0) {
            size = args != null && i < args.length ? size(args[i]) : -1;
        } else if (args != null) {
            for (Object a : args) {
                size = size(a);
                if (size >= 0) {
                    break;
                }
            }
        }
        call.state = size >= 0 ? Long.valueOf(size) : null;
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (call.state instanceof Long && error == null) {
            Hooks.complexity0(call.site, (Long) call.state, System.nanoTime() - call.start);
        }
    }

    /** Size of a value, or -1 if it has none. */
    static long size(Object v) {
        if (v == null) {
            return -1;
        }
        if (v instanceof Collection) {
            return ((Collection<?>) v).size();
        }
        if (v instanceof Map) {
            return ((Map<?, ?>) v).size();
        }
        if (v instanceof CharSequence) {
            return ((CharSequence) v).length();
        }
        if (v instanceof Number) {
            long n = ((Number) v).longValue();
            return n < 0 ? -1 : n;
        }
        if (v.getClass().isArray()) {
            return Array.getLength(v);
        }
        return -1;
    }
}
