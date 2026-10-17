package dev.openprofiler.boot;

import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.function.Function;

/**
 * A thread-safe map with weak, identity-compared keys. Values of collected keys can be drained.
 * Probes use it on every intercepted call from every thread, so it is split into lock stripes
 * by identity hash.
 */
final class WeakIdentityMap<K, V> {
    private static final int STRIPES = 16;

    @SuppressWarnings({"unchecked", "rawtypes"})
    private final Map<Object, V>[] maps = new Map[STRIPES];
    private final ReferenceQueue<Object> queue = new ReferenceQueue<>();

    WeakIdentityMap() {
        for (int i = 0; i < STRIPES; i++) {
            maps[i] = new HashMap<>();
        }
    }

    private static final class WeakKey extends WeakReference<Object> {
        final int hash;

        WeakKey(Object o, int hash, ReferenceQueue<Object> q) {
            super(o, q);
            this.hash = hash;
        }

        @Override
        public int hashCode() {
            return hash;
        }

        @Override
        public boolean equals(Object other) {
            if (this == other) {
                return true;
            }
            Object mine = get();
            if (mine == null) {
                return false;
            }
            if (other instanceof WeakKey) {
                return mine == ((WeakKey) other).get();
            }
            return other instanceof Lookup && mine == ((Lookup) other).o;
        }
    }

    private static final class Lookup {
        final Object o;
        final int hash;

        Lookup(Object o, int hash) {
            this.o = o;
            this.hash = hash;
        }

        @Override
        public int hashCode() {
            return hash;
        }

        @Override
        public boolean equals(Object other) {
            return other instanceof WeakKey && ((WeakKey) other).get() == o;
        }
    }

    private Map<Object, V> stripe(int hash) {
        return maps[(hash ^ (hash >>> 16)) & (STRIPES - 1)];
    }

    V get(K key) {
        int h = System.identityHashCode(key);
        Map<Object, V> m = stripe(h);
        synchronized (m) {
            return m.get(new Lookup(key, h));
        }
    }

    void put(K key, V value) {
        int h = System.identityHashCode(key);
        Map<Object, V> m = stripe(h);
        synchronized (m) {
            m.put(new WeakKey(key, h, queue), value);
        }
    }

    /** The value of {@code key}, created with {@code create} if there is none. */
    V computeIfAbsent(K key, Function<K, V> create) {
        int h = System.identityHashCode(key);
        Map<Object, V> m = stripe(h);
        synchronized (m) {
            V v = m.get(new Lookup(key, h));
            if (v == null) {
                v = create.apply(key);
                m.put(new WeakKey(key, h, queue), v);
            }
            return v;
        }
    }

    V remove(K key) {
        int h = System.identityHashCode(key);
        Map<Object, V> m = stripe(h);
        synchronized (m) {
            return m.remove(new Lookup(key, h));
        }
    }

    /** Removes entries whose keys were collected and returns their values. */
    List<V> expunge() {
        List<V> out = null;
        for (Object r; (r = queue.poll()) != null; ) {
            Map<Object, V> m = stripe(((WeakKey) r).hash);
            V v;
            synchronized (m) {
                v = m.remove(r);
            }
            if (v != null) {
                if (out == null) {
                    out = new ArrayList<>();
                }
                out.add(v);
            }
        }
        return out == null ? List.of() : out;
    }

    int size() {
        int n = 0;
        for (Map<Object, V> m : maps) {
            synchronized (m) {
                n += m.size();
            }
        }
        return n;
    }
}
