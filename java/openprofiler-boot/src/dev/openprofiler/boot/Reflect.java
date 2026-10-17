package dev.openprofiler.boot;

import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Reflective calls on application objects for probes (the boot classes cannot link against
 * libraries). Methods are looked up on accessible public types (an implementation class may be
 * hidden in a non-exported package while its interface is public), cached per class without
 * keeping class loaders alive. Every failure yields null.
 */
final class Reflect {
    private static final Object MISSING = new Object();

    private static final ClassValue<ConcurrentHashMap<String, Object>> CACHE =
        new ClassValue<ConcurrentHashMap<String, Object>>() {
            @Override
            protected ConcurrentHashMap<String, Object> computeValue(Class<?> type) {
                return new ConcurrentHashMap<>();
            }
        };

    private Reflect() {}

    /** {@code target.name()}, or null. */
    static Object call(Object target, String name) {
        return call(target, name, 0, null);
    }

    /** {@code target.name(arg)} for a one-parameter method, or null. */
    static Object call1(Object target, String name, Object arg) {
        return call(target, name, 1, new Object[] {arg});
    }

    static String str(Object target, String name) {
        Object v = call(target, name);
        return v == null ? null : String.valueOf(v);
    }

    private static Object call(Object target, String name, int argc, Object[] args) {
        if (target == null) {
            return null;
        }
        String key = name + "/" + argc;
        ConcurrentHashMap<String, Object> cache = CACHE.get(target.getClass());
        Object m = cache.get(key);
        if (m == null) {
            m = find(target.getClass(), name, argc);
            cache.put(key, m == null ? MISSING : m);
        }
        if (m == MISSING) {
            return null;
        }
        try {
            return ((Method) m).invoke(target, args);
        } catch (Throwable t) {
            return null;
        }
    }

    /** Value of a (possibly private) field declared in the class of {@code target} or a superclass. */
    static Object field(Object target, String name) {
        if (target == null) {
            return null;
        }
        ConcurrentHashMap<String, Object> cache = CACHE.get(target.getClass());
        String key = "#" + name;
        Object f = cache.get(key);
        if (f == null) {
            f = MISSING;
            for (Class<?> c = target.getClass(); c != null && f == MISSING; c = c.getSuperclass()) {
                try {
                    Field d = c.getDeclaredField(name);
                    d.setAccessible(true);
                    f = d;
                } catch (Throwable ignored) {
                    // keep looking
                }
            }
            cache.put(key, f);
        }
        try {
            return f == MISSING ? null : ((Field) f).get(target);
        } catch (Throwable t) {
            return null;
        }
    }

    private static boolean accessible(Class<?> c) {
        return Modifier.isPublic(c.getModifiers()) && c.getModule().isExported(c.getPackageName());
    }

    private static Method find(Class<?> type, String name, int argc) {
        for (Class<?> c = type; c != null; c = c.getSuperclass()) {
            Method m = declared(c, name, argc);
            if (m != null) {
                return m;
            }
            m = inInterfaces(c, name, argc);
            if (m != null) {
                return m;
            }
        }
        // A public method of a class in an open (unnamed) module.
        for (Method m : type.getMethods()) {
            if (m.getName().equals(name) && m.getParameterCount() == argc) {
                try {
                    m.setAccessible(true);
                    return m;
                } catch (Throwable t) {
                    return null;
                }
            }
        }
        return null;
    }

    private static Method inInterfaces(Class<?> c, String name, int argc) {
        for (Class<?> i : c.getInterfaces()) {
            Method m = declared(i, name, argc);
            if (m == null) {
                m = inInterfaces(i, name, argc);
            }
            if (m != null) {
                return m;
            }
        }
        return null;
    }

    private static Method declared(Class<?> c, String name, int argc) {
        if (!accessible(c)) {
            return null;
        }
        try {
            for (Method m : c.getMethods()) {
                if (m.getName().equals(name) && m.getParameterCount() == argc
                        && accessible(m.getDeclaringClass())) {
                    return m;
                }
            }
        } catch (Throwable ignored) {
            // fall through
        }
        return null;
    }

    /** Whether {@code o}'s class is or extends/implements a type named {@code name}. */
    static boolean isA(Object o, String name) {
        if (o == null) {
            return false;
        }
        for (Class<?> c = o.getClass(); c != null; c = c.getSuperclass()) {
            if (c.getName().equals(name) || implementsNamed(c, name)) {
                return true;
            }
        }
        return false;
    }

    private static boolean implementsNamed(Class<?> c, String name) {
        for (Class<?> i : c.getInterfaces()) {
            if (i.getName().equals(name) || implementsNamed(i, name)) {
                return true;
            }
        }
        return false;
    }

    /** {@code s} cut to {@code max} characters. */
    static String cut(String s, int max) {
        return s == null || s.length() <= max ? s : s.substring(0, max) + "…";
    }
}
