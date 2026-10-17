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

    /**
     * No-argument methods, one-argument methods and fields by name; plain names as keys, so a
     * lookup allocates nothing.
     */
    private static final ClassValue<ConcurrentHashMap<String, Object>> METHODS0 = cache();
    private static final ClassValue<ConcurrentHashMap<String, Object>> METHODS1 = cache();
    private static final ClassValue<ConcurrentHashMap<String, Object>> FIELDS = cache();

    private static ClassValue<ConcurrentHashMap<String, Object>> cache() {
        return new ClassValue<ConcurrentHashMap<String, Object>>() {
            @Override
            protected ConcurrentHashMap<String, Object> computeValue(Class<?> type) {
                return new ConcurrentHashMap<>();
            }
        };
    }

    private Reflect() {}

    /** {@code target.name()}, or null. */
    static Object call(Object target, String name) {
        return call(target, name, 0, null);
    }

    /** {@code target.name(arg)} for a one-parameter method, or null. */
    static Object call1(Object target, String name, Object arg) {
        return call(target, name, 1, new Object[] {arg});
    }

    /** {@code target.name(arg)} for a one-parameter method; whether it ran without throwing. */
    static boolean tryCall1(Object target, String name, Object arg) {
        if (target == null) {
            return false;
        }
        ConcurrentHashMap<String, Object> cache = METHODS1.get(target.getClass());
        Object m = cache.get(name);
        if (m == null) {
            m = find(target.getClass(), name, 1);
            cache.put(name, m == null ? MISSING : m);
        }
        if (m == MISSING) {
            return false;
        }
        try {
            ((Method) m).invoke(target, arg);
            return true;
        } catch (Throwable t) {
            return false;
        }
    }

    static String str(Object target, String name) {
        Object v = call(target, name);
        return v == null ? null : String.valueOf(v);
    }

    private static Object call(Object target, String name, int argc, Object[] args) {
        if (target == null) {
            return null;
        }
        ConcurrentHashMap<String, Object> cache = (argc == 0 ? METHODS0 : METHODS1).get(target.getClass());
        Object m = cache.get(name);
        if (m == null) {
            m = find(target.getClass(), name, argc);
            cache.put(name, m == null ? MISSING : m);
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
        ConcurrentHashMap<String, Object> cache = FIELDS.get(target.getClass());
        Object f = cache.get(name);
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
            cache.put(name, f);
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
        ConcurrentHashMap<String, Boolean> known = IS_A.get(o.getClass());
        Boolean r = known.get(name);
        if (r == null) {
            r = false;
            for (Class<?> c = o.getClass(); c != null && !r; c = c.getSuperclass()) {
                r = c.getName().equals(name) || implementsNamed(c, name);
            }
            known.put(name, r);
        }
        return r;
    }

    private static final ClassValue<ConcurrentHashMap<String, Boolean>> IS_A =
        new ClassValue<ConcurrentHashMap<String, Boolean>>() {
            @Override
            protected ConcurrentHashMap<String, Boolean> computeValue(Class<?> type) {
                return new ConcurrentHashMap<>();
            }
        };

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
