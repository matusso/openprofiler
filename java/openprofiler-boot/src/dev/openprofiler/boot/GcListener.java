package dev.openprofiler.boot;

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.Collection;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;
import javax.management.openmbean.TabularData;

/**
 * Forwards HotSpot GC notifications ({@code com.sun.management.gc.notification}) to the agent.
 * Reads the notification's open-type payload directly so no {@code com.sun.management} types are
 * needed at compile time.
 */
final class GcListener implements NotificationListener {
    private static final String GC_NOTIFICATION = "com.sun.management.gc.notification";

    /** JVM start in epoch millis; GC info times are relative to it. */
    private final long vmStartMillis = ManagementFactory.getRuntimeMXBean().getStartTime();

    static void install() {
        GcListener listener = new GcListener();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (gc instanceof NotificationEmitter) {
                try {
                    ((NotificationEmitter) gc).addNotificationListener(listener, null, null);
                } catch (Throwable ignored) {
                    // Not all collectors emit notifications.
                }
            }
        }
    }

    @Override
    public void handleNotification(Notification n, Object handback) {
        if (!GC_NOTIFICATION.equals(n.getType()) || !(n.getUserData() instanceof CompositeData)) {
            return;
        }
        try {
            CompositeData data = (CompositeData) n.getUserData();
            String name = String.valueOf(data.get("gcName"));
            String action = String.valueOf(data.get("gcAction"));
            String cause = String.valueOf(data.get("gcCause"));
            CompositeData info = (CompositeData) data.get("gcInfo");
            long start = ((Number) info.get("startTime")).longValue();
            long duration = ((Number) info.get("duration")).longValue();
            long before = used(info.get("memoryUsageBeforeGc"));
            long after = used(info.get("memoryUsageAfterGc"));
            // Concurrent cycles ("end of major GC" for ZGC/Shenandoah cycles) are not pauses.
            boolean pause = !name.contains("Cycles") && !action.contains("concurrent");
            long durationNanos = duration * 1_000_000L;
            Agent.gcEvent0((vmStartMillis + start) * 1_000_000L, durationNanos, name, cause, before, after,
                    pause ? durationNanos : 0L);
        } catch (Throwable ignored) {
            // Never propagate into the notification thread.
        }
    }

    private static long used(Object usageMap) {
        long total = 0;
        if (usageMap instanceof TabularData) {
            Collection<?> rows = ((TabularData) usageMap).values();
            for (Object row : rows) {
                CompositeData entry = (CompositeData) row;
                Object value = entry.get("value");
                if (value instanceof CompositeData) {
                    Object used = ((CompositeData) value).get("used");
                    if (used instanceof Number) {
                        total += ((Number) used).longValue();
                    }
                }
            }
        }
        return total;
    }
}
