package dev.ember.boot;

import java.io.File;
import java.lang.management.LockInfo;
import java.lang.management.ManagementFactory;
import java.lang.management.MonitorInfo;
import java.lang.management.PlatformManagedObject;
import java.lang.management.ThreadInfo;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.List;

/** Heap and thread dump helpers invoked by the native agent. */
final class Dumps {
    private Dumps() {}

    @SuppressWarnings("unchecked")
    private static Object hotspotDiagnostic() throws Exception {
        Class<?> iface = Class.forName("com.sun.management.HotSpotDiagnosticMXBean");
        return ManagementFactory.getPlatformMXBean((Class<PlatformManagedObject>) iface);
    }

    static String dumpHeap(String path, boolean live) {
        try {
            Object bean = hotspotDiagnostic();
            Method dump = Class.forName("com.sun.management.HotSpotDiagnosticMXBean")
                    .getMethod("dumpHeap", String.class, boolean.class);
            new File(path).delete();
            dump.invoke(bean, path, live);
            return null;
        } catch (Throwable t) {
            Throwable c = t.getCause() != null ? t.getCause() : t;
            return c.getClass().getName() + ": " + c.getMessage();
        }
    }

    /**
     * Line-oriented thread dump. Each thread starts with {@code T\tname\tid\tstate\tvirtual(0|1)},
     * followed by {@code F\tframe} lines (leaf first), {@code L\tlocked monitor} lines and an
     * optional {@code W\twaiting on} line.
     */
    static String[] threadDump() {
        List<String> out = new ArrayList<>();
        try {
            ThreadInfo[] infos = ManagementFactory.getThreadMXBean().dumpAllThreads(true, true);
            for (ThreadInfo ti : infos) {
                if (ti == null) {
                    continue;
                }
                out.add("T\t" + clean(ti.getThreadName()) + "\t" + ti.getThreadId() + "\t" + ti.getThreadState() + "\t0");
                StackTraceElement[] st = ti.getStackTrace();
                MonitorInfo[] monitors = ti.getLockedMonitors();
                for (int i = 0; i < st.length; i++) {
                    out.add("F\t" + clean(st[i].toString()));
                    for (MonitorInfo m : monitors) {
                        if (m.getLockedStackDepth() == i) {
                            out.add("L\t" + m.getClassName() + "@" + Integer.toHexString(m.getIdentityHashCode()));
                        }
                    }
                }
                for (LockInfo l : ti.getLockedSynchronizers()) {
                    out.add("L\t" + l.getClassName() + "@" + Integer.toHexString(l.getIdentityHashCode()));
                }
                LockInfo waiting = ti.getLockInfo();
                if (waiting != null) {
                    String owner = ti.getLockOwnerName() != null ? " owned by " + clean(ti.getLockOwnerName()) : "";
                    out.add("W\t" + waiting.getClassName() + "@" + Integer.toHexString(waiting.getIdentityHashCode()) + owner);
                }
            }
        } catch (Throwable ignored) {
            // Fall through with whatever was collected.
        }
        addVirtualThreads(out);
        return out.toArray(new String[0]);
    }

    /** JDK 21+: virtual threads are only visible through HotSpotDiagnosticMXBean.dumpThreads. */
    private static void addVirtualThreads(List<String> out) {
        File tmp = null;
        try {
            Object bean = hotspotDiagnostic();
            Class<?> iface = Class.forName("com.sun.management.HotSpotDiagnosticMXBean");
            Class<?> fmt = Class.forName("com.sun.management.HotSpotDiagnosticMXBean$ThreadDumpFormat");
            Method dump = iface.getMethod("dumpThreads", String.class, fmt);
            Object text = fmt.getField("TEXT_PLAIN").get(null);
            tmp = File.createTempFile("ember-threads", ".txt");
            tmp.delete();
            dump.invoke(bean, tmp.getAbsolutePath(), text);
            List<String> lines = Files.readAllLines(tmp.toPath(), StandardCharsets.UTF_8);
            boolean inVirtual = false;
            for (String line : lines) {
                if (line.startsWith("#")) {
                    inVirtual = line.trim().endsWith("virtual");
                    if (inVirtual) {
                        int sp = line.indexOf(' ');
                        String id = line.substring(1, sp < 0 ? line.length() : sp);
                        int q1 = line.indexOf('"');
                        int q2 = line.lastIndexOf('"');
                        String name = q1 >= 0 && q2 > q1 ? line.substring(q1 + 1, q2) : "";
                        out.add("T\t" + clean(name.isEmpty() ? "virtual-" + id : name) + "\t" + id + "\tVIRTUAL\t1");
                    }
                } else if (inVirtual && !line.trim().isEmpty()) {
                    out.add("F\t" + clean(line.trim()));
                } else if (line.trim().isEmpty()) {
                    inVirtual = false;
                }
            }
        } catch (Throwable ignored) {
            // Pre-21 JDKs or non-HotSpot VMs: platform threads only.
        } finally {
            if (tmp != null) {
                tmp.delete();
            }
        }
    }

    private static String clean(String s) {
        return s == null ? "" : s.replace('\t', ' ').replace('\n', ' ');
    }
}
