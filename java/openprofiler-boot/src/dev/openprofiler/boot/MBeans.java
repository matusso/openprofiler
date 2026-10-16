package dev.openprofiler.boot;

import java.lang.management.ManagementFactory;
import java.lang.reflect.Array;
import java.util.ArrayList;
import java.util.Collection;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeSet;
import java.util.concurrent.CopyOnWriteArrayList;
import javax.management.Attribute;
import javax.management.MBeanAttributeInfo;
import javax.management.MBeanInfo;
import javax.management.MBeanOperationInfo;
import javax.management.MBeanParameterInfo;
import javax.management.MBeanServer;
import javax.management.ObjectInstance;
import javax.management.ObjectName;
import javax.management.openmbean.CompositeData;
import javax.management.openmbean.TabularData;

/**
 * MBean browser helpers invoked by the native agent: listing, reading, invoking and setting
 * attributes on the platform MBean server, plus attributes sampled as telemetry.
 *
 * <p>Results are line-oriented string arrays with tab-separated fields (tabs and newlines in
 * fields are replaced by spaces). Values are JSON. A failure is a single {@code E\tmessage} line.
 */
final class MBeans {
    private static final int MAX_ITEMS = 1000;
    private static final int MAX_DEPTH = 8;

    /** Attributes sampled by {@link Telemetry}; append-only so telemetry column indices stay stable. */
    static final List<Tracked> TRACKED = new CopyOnWriteArrayList<>();

    /** Bumped whenever a tracked attribute is added or relabelled, so telemetry redeclares its columns. */
    static volatile int version;

    static final class Tracked {
        final String key;
        final ObjectName name;
        final String attribute;
        final String item;
        volatile String label;
        volatile int unit;
        volatile boolean active = true;

        Tracked(String key, ObjectName name, String attribute, String item, String label, int unit) {
            this.key = key;
            this.name = name;
            this.attribute = attribute;
            this.item = item;
            this.label = label;
            this.unit = unit;
        }

        /** The current value, NaN if it is missing or not numeric. */
        double sample() {
            try {
                Object v = server().getAttribute(name, attribute);
                if (!item.isEmpty()) {
                    v = v instanceof CompositeData ? ((CompositeData) v).get(item) : null;
                }
                if (v instanceof Number) {
                    return ((Number) v).doubleValue();
                }
                if (v instanceof Boolean) {
                    return ((Boolean) v) ? 1 : 0;
                }
            } catch (Throwable t) {
                // Bean unregistered or attribute failing: no value this second.
            }
            return Double.NaN;
        }
    }

    private MBeans() {}

    private static MBeanServer server() {
        return ManagementFactory.getPlatformMBeanServer();
    }

    private static String[] error(Throwable t) {
        Throwable c = t;
        while (c.getCause() != null && c.getCause() != c) {
            c = c.getCause();
        }
        String msg = c.getMessage();
        return new String[] {"E\t" + clean(c.getClass().getName() + (msg != null ? ": " + msg : ""))};
    }

    /** {@code B\tname\tclass\tdescription} per bean, then {@code T\t...} per tracked attribute. */
    static String[] list(String pattern) {
        List<String> out = new ArrayList<>();
        try {
            ObjectName p = pattern == null || pattern.isEmpty() ? null : new ObjectName(pattern);
            MBeanServer s = server();
            Set<String> seen = new TreeSet<>();
            List<String> rows = new ArrayList<>();
            for (ObjectInstance oi : s.queryMBeans(p, null)) {
                String name = oi.getObjectName().getCanonicalName();
                if (!seen.add(name)) {
                    continue;
                }
                String desc = "";
                try {
                    desc = s.getMBeanInfo(oi.getObjectName()).getDescription();
                } catch (Throwable t) {
                    // Keep the bean without a description.
                }
                rows.add("B\t" + clean(name) + "\t" + clean(oi.getClassName()) + "\t" + clean(desc));
            }
            rows.sort(null);
            out.addAll(rows);
        } catch (Throwable t) {
            return error(t);
        }
        addTracked(out);
        return out.toArray(new String[0]);
    }

    /**
     * {@code I\tname\tclass\tdescription}, then {@code A\tname\ttype\tdescription\tflags\tjson\terror}
     * per attribute (flags: {@code r} readable, {@code w} writable), then
     * {@code O\tname\treturnType\tdescription\timpact} per operation, each followed by its
     * {@code P\tname\ttype\tdescription} parameter lines.
     */
    static String[] get(String name) {
        List<String> out = new ArrayList<>();
        try {
            ObjectName on = new ObjectName(name);
            MBeanServer s = server();
            MBeanInfo info = s.getMBeanInfo(on);
            out.add("I\t" + clean(on.getCanonicalName()) + "\t" + clean(info.getClassName()) + "\t"
                    + clean(info.getDescription()));
            for (MBeanAttributeInfo a : info.getAttributes()) {
                String json = "";
                String err = "";
                if (a.isReadable()) {
                    try {
                        json = toJson(s.getAttribute(on, a.getName()));
                    } catch (Throwable t) {
                        err = error(t)[0].substring(2);
                    }
                }
                String flags = (a.isReadable() ? "r" : "") + (a.isWritable() ? "w" : "");
                out.add("A\t" + clean(a.getName()) + "\t" + clean(a.getType()) + "\t" + clean(a.getDescription())
                        + "\t" + flags + "\t" + json + "\t" + clean(err));
            }
            for (MBeanOperationInfo o : info.getOperations()) {
                out.add("O\t" + clean(o.getName()) + "\t" + clean(o.getReturnType()) + "\t"
                        + clean(o.getDescription()) + "\t" + impact(o.getImpact()));
                for (MBeanParameterInfo p : o.getSignature()) {
                    out.add("P\t" + clean(p.getName()) + "\t" + clean(p.getType()) + "\t" + clean(p.getDescription()));
                }
            }
        } catch (Throwable t) {
            return error(t);
        }
        return out.toArray(new String[0]);
    }

    /** {@code R\tjson} with the operation's result. */
    static String[] invoke(String name, String operation, String[] signature, String[] args) {
        try {
            if (signature.length != args.length) {
                throw new IllegalArgumentException(
                        "expected " + signature.length + " arguments, got " + args.length);
            }
            Object[] params = new Object[args.length];
            for (int i = 0; i < args.length; i++) {
                params[i] = convert(args[i], signature[i]);
            }
            Object r = server().invoke(new ObjectName(name), operation, params, signature);
            return new String[] {"R\t" + toJson(r)};
        } catch (Throwable t) {
            return error(t);
        }
    }

    /** Sets a writable attribute; returns no lines on success. */
    static String[] set(String name, String attribute, String value) {
        try {
            ObjectName on = new ObjectName(name);
            String type = null;
            for (MBeanAttributeInfo a : server().getMBeanInfo(on).getAttributes()) {
                if (a.getName().equals(attribute)) {
                    type = a.getType();
                }
            }
            if (type == null) {
                throw new IllegalArgumentException("no attribute " + attribute);
            }
            server().setAttribute(on, new Attribute(attribute, convert(value, type)));
            return new String[0];
        } catch (Throwable t) {
            return error(t);
        }
    }

    /** Starts or stops sampling an attribute as telemetry; returns the tracked list ({@code T} lines). */
    static synchronized String[] track(String name, String attribute, String item, String label, int unit, boolean enabled) {
        try {
            ObjectName on = new ObjectName(name);
            String canonical = on.getCanonicalName();
            String key = "mbean:" + canonical + "#" + attribute + (item.isEmpty() ? "" : "." + item);
            Tracked existing = null;
            for (Tracked t : TRACKED) {
                if (t.key.equals(key)) {
                    existing = t;
                }
            }
            if (enabled) {
                if (!server().isRegistered(on)) {
                    throw new IllegalArgumentException("no MBean " + canonical);
                }
                String l = label.isEmpty() ? defaultLabel(on, attribute, item) : label;
                if (existing == null) {
                    TRACKED.add(new Tracked(key, on, attribute, item, l, unit));
                } else {
                    existing.label = l;
                    existing.unit = unit;
                    existing.active = true;
                }
                version++;
            } else if (existing != null) {
                existing.active = false;
            }
        } catch (Throwable t) {
            return error(t);
        }
        List<String> out = new ArrayList<>();
        addTracked(out);
        return out.toArray(new String[0]);
    }

    private static void addTracked(List<String> out) {
        for (Tracked t : TRACKED) {
            if (t.active) {
                out.add("T\t" + clean(t.key) + "\t" + clean(t.label) + "\t" + t.unit + "\t"
                        + clean(t.name.getCanonicalName()) + "\t" + clean(t.attribute) + "\t" + clean(t.item));
            }
        }
    }

    private static String defaultLabel(ObjectName on, String attribute, String item) {
        String bean = on.getKeyProperty("name");
        if (bean == null) {
            bean = on.getKeyProperty("type");
        }
        String attr = attribute + (item.isEmpty() ? "" : "." + item);
        return bean == null ? attr : attr + " (" + bean + ")";
    }

    private static String impact(int i) {
        switch (i) {
            case MBeanOperationInfo.INFO:
                return "info";
            case MBeanOperationInfo.ACTION:
                return "action";
            case MBeanOperationInfo.ACTION_INFO:
                return "action_info";
            default:
                return "unknown";
        }
    }

    // ---- argument conversion -------------------------------------------------------------------

    static Object convert(String s, String type) throws Exception {
        if (type.startsWith("[")) {
            Class<?> component = arrayComponent(type);
            List<String> items = splitList(s);
            Object arr = Array.newInstance(component, items.size());
            for (int i = 0; i < items.size(); i++) {
                Array.set(arr, i, convert(items.get(i), component.getName()));
            }
            return arr;
        }
        switch (type) {
            case "java.lang.String":
                return s;
            case "int":
            case "java.lang.Integer":
                return Integer.valueOf(s.trim());
            case "long":
            case "java.lang.Long":
                return Long.valueOf(s.trim());
            case "double":
            case "java.lang.Double":
                return Double.valueOf(s.trim());
            case "float":
            case "java.lang.Float":
                return Float.valueOf(s.trim());
            case "short":
            case "java.lang.Short":
                return Short.valueOf(s.trim());
            case "byte":
            case "java.lang.Byte":
                return Byte.valueOf(s.trim());
            case "boolean":
            case "java.lang.Boolean": {
                String b = s.trim().toLowerCase();
                if (!b.equals("true") && !b.equals("false")) {
                    throw new IllegalArgumentException("not a boolean: " + s);
                }
                return Boolean.valueOf(b);
            }
            case "char":
            case "java.lang.Character":
                if (s.length() != 1) {
                    throw new IllegalArgumentException("not a single character: " + s);
                }
                return s.charAt(0);
            case "javax.management.ObjectName":
                return new ObjectName(s.trim());
            case "java.lang.Object":
                return s;
            default:
                throw new IllegalArgumentException("unsupported parameter type " + type);
        }
    }

    private static Class<?> arrayComponent(String type) throws ClassNotFoundException {
        switch (type) {
            case "[I":
                return int.class;
            case "[J":
                return long.class;
            case "[D":
                return double.class;
            case "[F":
                return float.class;
            case "[S":
                return short.class;
            case "[B":
                return byte.class;
            case "[Z":
                return boolean.class;
            case "[C":
                return char.class;
            default:
                if (type.startsWith("[L") && type.endsWith(";")) {
                    return Class.forName(type.substring(2, type.length() - 1));
                }
                throw new IllegalArgumentException("unsupported parameter type " + type);
        }
    }

    /** {@code ["a", "b"]} (JSON strings or bare values) or {@code a, b}. */
    private static List<String> splitList(String s) {
        List<String> out = new ArrayList<>();
        String t = s.trim();
        if (t.startsWith("[") && t.endsWith("]")) {
            t = t.substring(1, t.length() - 1);
        }
        if (t.trim().isEmpty()) {
            return out;
        }
        StringBuilder cur = new StringBuilder();
        boolean quoted = false;
        for (int i = 0; i < t.length(); i++) {
            char c = t.charAt(i);
            if (quoted) {
                if (c == '\\' && i + 1 < t.length()) {
                    cur.append(t.charAt(++i));
                } else if (c == '"') {
                    quoted = false;
                } else {
                    cur.append(c);
                }
            } else if (c == '"') {
                quoted = true;
            } else if (c == ',') {
                out.add(cur.toString().trim());
                cur.setLength(0);
            } else {
                cur.append(c);
            }
        }
        out.add(cur.toString().trim());
        return out;
    }

    // ---- JSON ------------------------------------------------------------------------------------

    static String toJson(Object v) {
        StringBuilder sb = new StringBuilder();
        json(sb, v, 0);
        return sb.toString();
    }

    private static void json(StringBuilder sb, Object v, int depth) {
        if (v == null) {
            sb.append("null");
        } else if (depth > MAX_DEPTH) {
            str(sb, String.valueOf(v));
        } else if (v instanceof Double || v instanceof Float) {
            double d = ((Number) v).doubleValue();
            if (Double.isNaN(d) || Double.isInfinite(d)) {
                str(sb, Double.toString(d));
            } else {
                sb.append(v);
            }
        } else if (v instanceof Number || v instanceof Boolean) {
            sb.append(v);
        } else if (v instanceof CompositeData) {
            CompositeData cd = (CompositeData) v;
            sb.append('{');
            boolean first = true;
            for (String k : cd.getCompositeType().keySet()) {
                if (!first) {
                    sb.append(',');
                }
                first = false;
                str(sb, k);
                sb.append(':');
                json(sb, cd.get(k), depth + 1);
            }
            sb.append('}');
        } else if (v instanceof TabularData) {
            seq(sb, ((TabularData) v).values(), depth);
        } else if (v instanceof Map) {
            sb.append('{');
            boolean first = true;
            int n = 0;
            for (Map.Entry<?, ?> e : ((Map<?, ?>) v).entrySet()) {
                if (n++ >= MAX_ITEMS) {
                    break;
                }
                if (!first) {
                    sb.append(',');
                }
                first = false;
                str(sb, String.valueOf(e.getKey()));
                sb.append(':');
                json(sb, e.getValue(), depth + 1);
            }
            sb.append('}');
        } else if (v instanceof Collection) {
            seq(sb, (Collection<?>) v, depth);
        } else if (v.getClass().isArray()) {
            int len = Array.getLength(v);
            List<Object> items = new ArrayList<>(Math.min(len, MAX_ITEMS));
            for (int i = 0; i < len && i < MAX_ITEMS; i++) {
                items.add(Array.get(v, i));
            }
            seq(sb, items, depth);
        } else if (v instanceof ObjectName) {
            str(sb, ((ObjectName) v).getCanonicalName());
        } else {
            str(sb, String.valueOf(v));
        }
    }

    private static void seq(StringBuilder sb, Collection<?> c, int depth) {
        sb.append('[');
        int n = 0;
        for (Object o : c) {
            if (n >= MAX_ITEMS) {
                break;
            }
            if (n++ > 0) {
                sb.append(',');
            }
            json(sb, o, depth + 1);
        }
        sb.append(']');
    }

    private static void str(StringBuilder sb, String s) {
        sb.append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"':
                    sb.append("\\\"");
                    break;
                case '\\':
                    sb.append("\\\\");
                    break;
                case '\n':
                    sb.append("\\n");
                    break;
                case '\r':
                    sb.append("\\r");
                    break;
                case '\t':
                    sb.append("\\t");
                    break;
                default:
                    if (c < 0x20) {
                        sb.append(String.format("\\u%04x", (int) c));
                    } else {
                        sb.append(c);
                    }
            }
        }
        sb.append('"');
    }

    private static String clean(String s) {
        return s == null ? "" : s.replace('\t', ' ').replace('\n', ' ').replace('\r', ' ');
    }
}
