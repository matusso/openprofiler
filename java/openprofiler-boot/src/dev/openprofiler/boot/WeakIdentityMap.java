package dev.openprofiler.boot;

import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/** A synchronized map with weak, identity-compared keys. Values of collected keys can be drained. */
final class WeakIdentityMap<K, V> {
    private final Map<Object, V> map = new HashMap<>();
    private final ReferenceQueue<Object> queue = new ReferenceQueue<>();

    private static final class WeakKey extends WeakReference<Object> {
        final int hash;

        WeakKey(Object o, ReferenceQueue<Object> q) {
            super(o, q);
            hash = System.identityHashCode(o);
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

        Lookup(Object o) {
            this.o = o;
        }

        @Override
        public int hashCode() {
            return System.identityHashCode(o);
        }

        @Override
        public boolean equals(Object other) {
            return other instanceof WeakKey && ((WeakKey) other).get() == o;
        }
    }

    synchronized V get(K key) {
        return map.get(new Lookup(key));
    }

    synchronized void put(K key, V value) {
        map.put(new WeakKey(key, queue), value);
    }

    synchronized V remove(K key) {
        return map.remove(new Lookup(key));
    }

    /** Removes entries whose keys were collected and returns their values. */
    synchronized List<V> expunge() {
        List<V> out = null;
        for (Object r; (r = queue.poll()) != null; ) {
            V v = map.remove(r);
            if (v != null) {
                if (out == null) {
                    out = new ArrayList<>();
                }
                out.add(v);
            }
        }
        return out == null ? List.of() : out;
    }

    synchronized int size() {
        return map.size();
    }
}
