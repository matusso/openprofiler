package dev.openprofiler.boot;

import java.io.IOException;
import java.lang.management.ManagementFactory;
import java.lang.reflect.Array;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.FutureTask;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import javax.management.Attribute;
import javax.management.AttributeChangeNotification;
import javax.management.MBeanAttributeInfo;
import javax.management.MBeanInfo;
import javax.management.MBeanNotificationInfo;
import javax.management.MBeanOperationInfo;
import javax.management.MBeanParameterInfo;
import javax.management.MBeanServer;
import javax.management.MBeanServerConnection;
import javax.management.MBeanServerFactory;
import javax.management.Notification;
import javax.management.NotificationBroadcaster;
import javax.management.NotificationListener;
import javax.management.ObjectInstance;
import javax.management.ObjectName;
import javax.management.openmbean.CompositeData;
import javax.management.openmbean.TabularData;
import javax.management.remote.JMXConnectionNotification;
import javax.management.remote.JMXConnector;
import javax.management.remote.JMXConnectorFactory;
import javax.management.remote.JMXServiceURL;

/**
 * MBean browser helpers invoked by the native agent through {@link #call}: listing, reading,
 * invoking and setting attributes, attributes sampled as telemetry, and buffered notifications.
 *
 * <p>Servers are addressed by id: empty for the platform MBean server, the delegate's
 * {@code MBeanServerId} for other MBean servers in this JVM ({@link MBeanServerFactory}), and the
 * service URL for remote JMX servers this JVM connected to ({@link #connect}).
 *
 * <p>Results are line-oriented string arrays with tab-separated fields (tabs and newlines in
 * fields are replaced by spaces). Values are JSON. A failure is a single {@code E\tmessage} line.
 */
final class MBeans {
    private static final int MAX_ITEMS = 1000;
    private static final int MAX_DEPTH = 8;
    private static final int MAX_NOTIFICATIONS = 1000;
    private static final long CONNECT_TIMEOUT_MS = 15_000;
    private static final ObjectName DELEGATE = delegateName();

    /** Attributes sampled by {@link Telemetry}; append-only so telemetry column indices stay stable. */
    static final List<Tracked> TRACKED = new CopyOnWriteArrayList<>();

    /** Bumped whenever a tracked attribute is added or relabelled, so telemetry redeclares its columns. */
    static volatile int version;

    /** Remote JMX connections by service URL. */
    private static final Map<String, Remote> REMOTES = new ConcurrentHashMap<>();

    private static final List<Subscription> SUBSCRIPTIONS = new CopyOnWriteArrayList<>();

    /** The last {@link #MAX_NOTIFICATIONS} notifications received; guarded by itself. */
    private static final ArrayDeque<Received> NOTIFICATIONS = new ArrayDeque<>();
    private static long lastSeq;

    /** Samples tracked attributes of remote servers, so a slow connection never stalls telemetry. */
    private static Thread poller;

    static final class Tracked {
        final String key;
        final String server;
        final ObjectName name;
        final String attribute;
        final String item;
        volatile String label;
        volatile int unit;
        volatile boolean active = true;
        /** The last value read by the poller (remote servers only). */
        volatile double polled = Double.NaN;

        Tracked(String key, String server, ObjectName name, String attribute, String item, String label, int unit) {
            this.key = key;
            this.server = server;
            this.name = name;
            this.attribute = attribute;
            this.item = item;
            this.label = label;
            this.unit = unit;
        }

        /** The current value, NaN if it is missing or not numeric. */
        double sample() {
            return REMOTES.containsKey(server) ? polled : read();
        }

        double read() {
            try {
                Object v = server(server).getAttribute(name, attribute);
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
                // Bean unregistered, server gone or attribute failing: no value this second.
            }
            return Double.NaN;
        }
    }

    private static final class Remote implements NotificationListener {
        final String url;
        final JMXConnector connector;
        final MBeanServerConnection conn;
        volatile String error;

        Remote(String url, JMXConnector connector, MBeanServerConnection conn) {
            this.url = url;
            this.connector = connector;
            this.conn = conn;
        }

        @Override
        public void handleNotification(Notification n, Object handback) {
            if (JMXConnectionNotification.FAILED.equals(n.getType())) {
                error = "connection failed";
            } else if (JMXConnectionNotification.CLOSED.equals(n.getType())) {
                error = "connection closed";
            }
        }
    }

    private static final class Subscription implements NotificationListener {
        final String server;
        final ObjectName name;
        final MBeanServerConnection conn;

        Subscription(String server, ObjectName name, MBeanServerConnection conn) {
            this.server = server;
            this.name = name;
            this.conn = conn;
        }

        @Override
        public void handleNotification(Notification n, Object handback) {
            record(this, n);
        }
    }

    private static final class Received {
        final long seq;
        final String line;

        Received(long seq, String line) {
            this.seq = seq;
            this.line = line;
        }
    }

    private MBeans() {}

    private static ObjectName delegateName() {
        try {
            return new ObjectName("JMImplementation:type=MBeanServerDelegate");
        } catch (Exception e) {
            throw new IllegalStateException(e);
        }
    }

    /**
     * Runs one operation; {@code args} are its parameters as strings:
     * {@code servers()}, {@code connect(url, user, password, enabled)}, {@code list(server, pattern)},
     * {@code get(server, name)}, {@code invoke(server, name, operation, n, n signature types, n args)},
     * {@code set(server, name, attribute, value)},
     * {@code track(server, name, attribute, item, label, unit, enabled)},
     * {@code subscribe(server, name, enabled)} and {@code notifications(after)}.
     */
    static String[] call(String op, String[] a) {
        try {
            switch (op) {
                case "servers":
                    return servers();
                case "connect":
                    return connect(a[0], a[1], a[2], Boolean.parseBoolean(a[3]));
                case "list":
                    return list(a[0], a[1]);
                case "get":
                    return get(a[0], a[1]);
                case "invoke": {
                    int n = Integer.parseInt(a[3]);
                    return invoke(a[0], a[1], a[2], Arrays.copyOfRange(a, 4, 4 + n),
                            Arrays.copyOfRange(a, 4 + n, a.length));
                }
                case "set":
                    return set(a[0], a[1], a[2], a[3]);
                case "track":
                    return track(a[0], a[1], a[2], a[3], a[4], Integer.parseInt(a[5]), Boolean.parseBoolean(a[6]));
                case "subscribe":
                    return subscribe(a[0], a[1], Boolean.parseBoolean(a[2]));
                case "notifications": {
                    long after = Long.parseUnsignedLong(a[0]);
                    return notifications(after < 0 ? Long.MAX_VALUE : after);
                }
                default:
                    throw new IllegalArgumentException("unknown MBean operation " + op);
            }
        } catch (Throwable t) {
            return error(t);
        }
    }

    // ---- servers -------------------------------------------------------------------------------

    static MBeanServerConnection server(String id) throws IOException {
        if (id == null || id.isEmpty()) {
            return ManagementFactory.getPlatformMBeanServer();
        }
        Remote r = REMOTES.get(id);
        if (r != null) {
            if (r.error != null) {
                throw new IOException(id + ": " + r.error);
            }
            return r.conn;
        }
        for (MBeanServer s : localServers()) {
            if (id.equals(serverId(s))) {
                return s;
            }
        }
        throw new IllegalArgumentException("no MBean server " + id);
    }

    /** MBean servers created through {@link MBeanServerFactory}, except the platform one. */
    private static List<MBeanServer> localServers() {
        MBeanServer platform = ManagementFactory.getPlatformMBeanServer();
        List<MBeanServer> out = new ArrayList<>();
        for (MBeanServer s : MBeanServerFactory.findMBeanServer(null)) {
            if (s != platform) {
                out.add(s);
            }
        }
        return out;
    }

    private static String serverId(MBeanServerConnection s) {
        try {
            return String.valueOf(s.getAttribute(DELEGATE, "MBeanServerId"));
        } catch (Throwable t) {
            return "";
        }
    }

    /** {@code S\tid\tkind\tlabel\tdefaultDomain\tbeanCount\terror} per server (platform, local, remote). */
    static String[] servers() {
        List<String> out = new ArrayList<>();
        out.add(serverLine("", "platform", "Platform MBean server", ManagementFactory.getPlatformMBeanServer(), null));
        for (MBeanServer s : localServers()) {
            String id = serverId(s);
            out.add(serverLine(id, "local", id, s, null));
        }
        for (Remote r : new TreeMap<>(REMOTES).values()) {
            out.add(serverLine(r.url, "remote", r.url, r.conn, r.error));
        }
        return out.toArray(new String[0]);
    }

    private static String serverLine(String id, String kind, String label, MBeanServerConnection s, String error) {
        String domain = "";
        String count = "";
        if (error == null) {
            try {
                domain = s.getDefaultDomain();
                count = String.valueOf(s.getMBeanCount());
            } catch (Throwable t) {
                error = message(t);
            }
        }
        return "S\t" + clean(id) + "\t" + kind + "\t" + clean(label) + "\t" + clean(domain) + "\t" + count + "\t"
                + clean(error);
    }

    /**
     * Connects to (or, with {@code enabled} false, disconnects from) a remote JMX server; the
     * credentials are only kept by the connector. Returns {@link #servers()}.
     */
    static String[] connect(String url, String user, String password, boolean enabled) throws Exception {
        String id = url.trim();
        Remote old = REMOTES.remove(id);
        if (old != null) {
            SUBSCRIPTIONS.removeIf(s -> s.server.equals(id));
            Thread t = new Thread(() -> {
                try {
                    old.connector.close();
                } catch (Throwable e) {
                    // Already broken.
                }
            }, "OpenProfiler JMX Close");
            t.setDaemon(true);
            t.start();
        }
        if (enabled) {
            JMXServiceURL u = new JMXServiceURL(id);
            Map<String, Object> env = new HashMap<>();
            if (!user.isEmpty()) {
                env.put(JMXConnector.CREDENTIALS, new String[] {user, password});
            }
            FutureTask<JMXConnector> task = new FutureTask<>(() -> JMXConnectorFactory.connect(u, env));
            Thread t = new Thread(task, "OpenProfiler JMX Connect");
            t.setDaemon(true);
            t.start();
            JMXConnector c;
            try {
                c = task.get(CONNECT_TIMEOUT_MS, TimeUnit.MILLISECONDS);
            } catch (TimeoutException e) {
                task.cancel(true);
                throw new IOException("timed out connecting to " + id);
            }
            Remote r = new Remote(id, c, c.getMBeanServerConnection());
            c.addConnectionNotificationListener(r, null, null);
            REMOTES.put(id, r);
        }
        return servers();
    }

    // ---- browsing ------------------------------------------------------------------------------

    private static String[] error(Throwable t) {
        return new String[] {"E\t" + clean(message(t))};
    }

    private static String message(Throwable t) {
        Throwable c = t;
        while (c.getCause() != null && c.getCause() != c) {
            c = c.getCause();
        }
        String msg = c.getMessage();
        return c.getClass().getName() + (msg != null ? ": " + msg : "");
    }

    /** {@code B\tname\tclass\tdescription} per bean, then {@code T\t...} per tracked attribute. */
    static String[] list(String server, String pattern) throws Exception {
        List<String> out = new ArrayList<>();
        ObjectName p = pattern == null || pattern.isEmpty() ? null : new ObjectName(pattern);
        MBeanServerConnection s = server(server);
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
        addTracked(out);
        return out.toArray(new String[0]);
    }

    /**
     * {@code I\tname\tclass\tdescription}, then {@code A\tname\ttype\tdescription\tflags\tjson\terror}
     * per attribute (flags: {@code r} readable, {@code w} writable), then
     * {@code O\tname\treturnType\tdescription\timpact} per operation, each followed by its
     * {@code P\tname\ttype\tdescription} parameter lines, then {@code N\tclass\tdescription\ttypes}
     * per declared notification (types comma-separated).
     */
    static String[] get(String server, String name) throws Exception {
        List<String> out = new ArrayList<>();
        ObjectName on = new ObjectName(name);
        MBeanServerConnection s = server(server);
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
                    err = message(t);
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
        for (MBeanNotificationInfo n : info.getNotifications()) {
            out.add("N\t" + clean(n.getName()) + "\t" + clean(n.getDescription()) + "\t"
                    + clean(String.join(",", n.getNotifTypes())));
        }
        return out.toArray(new String[0]);
    }

    /** {@code R\tjson} with the operation's result. */
    static String[] invoke(String server, String name, String operation, String[] signature, String[] args)
            throws Exception {
        if (signature.length != args.length) {
            throw new IllegalArgumentException("expected " + signature.length + " arguments, got " + args.length);
        }
        Object[] params = new Object[args.length];
        for (int i = 0; i < args.length; i++) {
            params[i] = convert(args[i], signature[i]);
        }
        Object r = server(server).invoke(new ObjectName(name), operation, params, signature);
        return new String[] {"R\t" + toJson(r)};
    }

    /** Sets a writable attribute; returns no lines on success. */
    static String[] set(String server, String name, String attribute, String value) throws Exception {
        ObjectName on = new ObjectName(name);
        MBeanServerConnection s = server(server);
        String type = null;
        for (MBeanAttributeInfo a : s.getMBeanInfo(on).getAttributes()) {
            if (a.getName().equals(attribute)) {
                type = a.getType();
            }
        }
        if (type == null) {
            throw new IllegalArgumentException("no attribute " + attribute);
        }
        s.setAttribute(on, new Attribute(attribute, convert(value, type)));
        return new String[0];
    }

    // ---- telemetry -----------------------------------------------------------------------------

    /** Starts or stops sampling an attribute as telemetry; returns the tracked list ({@code T} lines). */
    static synchronized String[] track(String server, String name, String attribute, String item, String label,
                                       int unit, boolean enabled) throws Exception {
        ObjectName on = new ObjectName(name);
        String canonical = on.getCanonicalName();
        String key = "mbean:" + (server.isEmpty() ? "" : "[" + server + "]") + canonical + "#" + attribute
                + (item.isEmpty() ? "" : "." + item);
        Tracked existing = null;
        for (Tracked t : TRACKED) {
            if (t.key.equals(key)) {
                existing = t;
            }
        }
        if (enabled) {
            if (!server(server).isRegistered(on)) {
                throw new IllegalArgumentException("no MBean " + canonical);
            }
            String l = label.isEmpty() ? defaultLabel(on, attribute, item) : label;
            if (existing == null) {
                TRACKED.add(new Tracked(key, server, on, attribute, item, l, unit));
            } else {
                existing.label = l;
                existing.unit = unit;
                existing.active = true;
            }
            if (REMOTES.containsKey(server) && poller == null) {
                poller = new Thread(MBeans::poll, "OpenProfiler JMX Poller");
                poller.setDaemon(true);
                poller.start();
            }
            version++;
        } else if (existing != null) {
            existing.active = false;
        }
        List<String> out = new ArrayList<>();
        addTracked(out);
        return out.toArray(new String[0]);
    }

    private static void poll() {
        while (true) {
            for (Tracked t : TRACKED) {
                if (t.active && REMOTES.containsKey(t.server)) {
                    t.polled = t.read();
                }
            }
            try {
                Thread.sleep(1000);
            } catch (InterruptedException e) {
                return;
            }
        }
    }

    /** {@code T\tkey\tlabel\tunit\tname\tattribute\titem\tserver} per active tracked attribute. */
    private static void addTracked(List<String> out) {
        for (Tracked t : TRACKED) {
            if (t.active) {
                out.add("T\t" + clean(t.key) + "\t" + clean(t.label) + "\t" + t.unit + "\t"
                        + clean(t.name.getCanonicalName()) + "\t" + clean(t.attribute) + "\t" + clean(t.item)
                        + "\t" + clean(t.server));
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

    // ---- notifications -------------------------------------------------------------------------

    /** Adds or removes a listener on one MBean; returns {@link #notifications} with no notifications. */
    static synchronized String[] subscribe(String server, String name, boolean enabled) throws Exception {
        ObjectName on = new ObjectName(name);
        Subscription existing = null;
        for (Subscription s : SUBSCRIPTIONS) {
            if (s.server.equals(server) && s.name.equals(on)) {
                existing = s;
            }
        }
        if (enabled && existing == null) {
            if (on.isPattern()) {
                throw new IllegalArgumentException("not a single MBean: " + name);
            }
            MBeanServerConnection c = server(server);
            if (!c.isInstanceOf(on, NotificationBroadcaster.class.getName())) {
                throw new IllegalArgumentException(on.getCanonicalName() + " does not emit notifications");
            }
            Subscription s = new Subscription(server, on, c);
            c.addNotificationListener(on, s, null, null);
            SUBSCRIPTIONS.add(s);
        } else if (!enabled && existing != null) {
            SUBSCRIPTIONS.remove(existing);
            try {
                existing.conn.removeNotificationListener(on, existing);
            } catch (Throwable t) {
                // Bean unregistered or connection gone.
            }
        }
        return notifications(Long.MAX_VALUE);
    }

    private static void record(Subscription s, Notification n) {
        String data = "";
        try {
            if (n instanceof AttributeChangeNotification) {
                AttributeChangeNotification a = (AttributeChangeNotification) n;
                StringBuilder sb = new StringBuilder("{\"attribute\":");
                str(sb, String.valueOf(a.getAttributeName()));
                sb.append(",\"type\":");
                str(sb, String.valueOf(a.getAttributeType()));
                sb.append(",\"old\":");
                json(sb, a.getOldValue(), 1);
                sb.append(",\"new\":");
                json(sb, a.getNewValue(), 1);
                data = sb.append('}').toString();
            } else if (n.getUserData() != null) {
                data = toJson(n.getUserData());
            }
        } catch (Throwable t) {
            // Keep the notification without data.
        }
        long time = n.getTimeStamp() > 0 ? n.getTimeStamp() : System.currentTimeMillis();
        synchronized (NOTIFICATIONS) {
            long seq = ++lastSeq;
            NOTIFICATIONS.addLast(new Received(seq, "N\t" + seq + "\t" + time + "\t" + clean(s.server) + "\t"
                    + clean(s.name.getCanonicalName()) + "\t" + clean(n.getType()) + "\t" + n.getSequenceNumber()
                    + "\t" + clean(n.getMessage()) + "\t" + data));
            if (NOTIFICATIONS.size() > MAX_NOTIFICATIONS) {
                NOTIFICATIONS.removeFirst();
            }
        }
    }

    /**
     * {@code N\tseq\ttimeMillis\tserver\tsource\ttype\tsequenceNumber\tmessage\tdataJson} per buffered
     * notification with a sequence number above {@code after}, {@code U\tserver\tname} per
     * subscription, then {@code L\tlastSeq\tmissed} ({@code missed}: notifications after
     * {@code after} that no longer fit the buffer).
     */
    static String[] notifications(long after) {
        List<String> out = new ArrayList<>();
        long last;
        long missed;
        synchronized (NOTIFICATIONS) {
            last = lastSeq;
            long oldest = NOTIFICATIONS.isEmpty() ? last + 1 : NOTIFICATIONS.peekFirst().seq;
            missed = Math.max(0, oldest - 1 - after);
            for (Received r : NOTIFICATIONS) {
                if (r.seq > after) {
                    out.add(r.line);
                }
            }
        }
        for (Subscription s : SUBSCRIPTIONS) {
            out.add("U\t" + clean(s.server) + "\t" + clean(s.name.getCanonicalName()));
        }
        out.add("L\t" + last + "\t" + missed);
        return out.toArray(new String[0]);
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
