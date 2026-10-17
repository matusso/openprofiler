package dev.openprofiler.boot;

/**
 * Carries the current request label across thread hand-offs, so probe events and call trees on
 * worker threads are attributed to the request that caused the work. Work is tagged where it is
 * created or submitted and the label is restored around its execution:
 *
 * <ul>
 *   <li>{@code FutureTask} (executor {@code submit}, scheduled executors, Reactor schedulers);
 *   <li>{@code ForkJoinTask} (fork/join, parallel streams, {@code CompletableFuture} async stages,
 *       also when they run on other executors);
 *   <li>{@code ThreadPoolExecutor.execute} of plain runnables (restored in {@code beforeExecute});
 *   <li>{@code Thread.start} (platform threads and, through {@code VirtualThread.run}, virtual threads);
 *   <li>Kotlin coroutines ({@code CoroutineDispatcher.dispatch}, {@code DispatchedTask.run});
 *   <li>JBoss Threads' {@code EnhancedQueueExecutor} (Quarkus' and WildFly's worker pools).
 * </ul>
 */
final class AsyncProbe extends Probe {
    private static final int FUTURE_INIT = 0, FUTURE_RUN = 1, FUTURE_RUN_RESET = 2, FJ_INIT = 3, FJ_EXEC = 4,
        CF_ASYNC_RUN = 5, CF_ASYNC_SUPPLY = 6, CF_COMPLETION = 7, TPE_EXECUTE = 8, TPE_BEFORE = 9,
        TPE_AFTER = 10, THREAD_START = 11, THREAD_RUN = 12, VTHREAD_RUN = 13, KT_DISPATCH = 14, KT_RUN = 15,
        EQE_INIT = 16, EQE_RUN = 17;

    private final WeakIdentityMap<Object, String> labels = new WeakIdentityMap<>();
    /** No label was ever captured: skip the map entirely. */
    private volatile boolean any;

    AsyncProbe() {
        super("async", "Async context", "Task", null);
    }

    @Override
    boolean visible() {
        return false;
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.method("java.util.concurrent.FutureTask", "<init>", null).exitOnly(),
            Interception.method("java.util.concurrent.FutureTask", "run", "()V"),
            Interception.method("java.util.concurrent.FutureTask", "runAndReset", "()Z"),
            Interception.method("java.util.concurrent.ForkJoinTask", "<init>", "()V").exitOnly(),
            Interception.method("java.util.concurrent.ForkJoinTask", "doExec", "()I"),
            Interception.method("java.util.concurrent.CompletableFuture$AsyncRun", "run", "()V"),
            Interception.method("java.util.concurrent.CompletableFuture$AsyncSupply", "run", "()V"),
            Interception.subtypes("java.util.concurrent.CompletableFuture$Completion", "run", "()V"),
            Interception.method("java.util.concurrent.ThreadPoolExecutor", "execute", "(Ljava/lang/Runnable;)V")
                .withArgs().entryOnly(),
            Interception.method("java.util.concurrent.ThreadPoolExecutor", "beforeExecute",
                                "(Ljava/lang/Thread;Ljava/lang/Runnable;)V").withArgs().entryOnly(),
            Interception.method("java.util.concurrent.ThreadPoolExecutor", "afterExecute",
                                "(Ljava/lang/Runnable;Ljava/lang/Throwable;)V").withArgs().entryOnly(),
            Interception.method("java.lang.Thread", "start", "()V").entryOnly(),
            Interception.method("java.lang.Thread", "run", "()V"),
            Interception.method("java.lang.VirtualThread", "run", "(Ljava/lang/Runnable;)V"),
            Interception.subtypes("kotlinx.coroutines.CoroutineDispatcher", "dispatch",
                                  "(Lkotlin/coroutines/CoroutineContext;Ljava/lang/Runnable;)V").withArgs().entryOnly(),
            Interception.method("kotlinx.coroutines.DispatchedTask", "run", "()V"),
            Interception.method("org.jboss.threads.EnhancedQueueExecutor$Task", "<init>", null).exitOnly(),
            Interception.method("org.jboss.threads.EnhancedQueueExecutor$Task", "run", "()V"),
        };
    }

    private void capture(Object task) {
        ProbeThread t = ProbeThread.current();
        if (task != null && t != null && t.context != null) {
            any = true;
            labels.expunge();
            labels.put(task, t.context);
        }
    }

    /**
     * Restores the label captured for {@code task}. Tasks that run once drop their entry
     * ({@code once}), so the map's weak reference never needs the GC to clear it.
     */
    private void restore(Call call, Object task, boolean once) {
        String label = task == null || !any ? null : once ? labels.remove(task) : labels.get(task);
        if (label != null) {
            label(call, label);
        }
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        switch (call.interception) {
            case FUTURE_RUN:
            case CF_ASYNC_RUN:
            case CF_ASYNC_SUPPLY:
            case THREAD_RUN:
            case VTHREAD_RUN:
            case EQE_RUN:
                restore(call, self, true);
                break;
            case FUTURE_RUN_RESET:
            case FJ_EXEC:
            case CF_COMPLETION:
            case KT_RUN:
                // Periodic tasks, completions that may also fire directly, reused continuations.
                restore(call, self, false);
                break;
            case TPE_EXECUTE:
                capture(args[0]);
                break;
            case TPE_BEFORE: {
                ProbeThread t = ProbeThread.current();
                String label = any ? labels.remove(args[1]) : null;
                if (t != null && label != null) {
                    t.setContext(label);
                    t.pooledContext = true;
                }
                break;
            }
            case TPE_AFTER: {
                ProbeThread t = ProbeThread.current();
                if (t != null && t.pooledContext) {
                    t.pooledContext = false;
                    t.setContext(null);
                }
                break;
            }
            case THREAD_START:
                capture(self);
                break;
            case KT_DISPATCH:
                capture(args[1]);
                break;
            default:
                break;
        }
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (call.interception == FUTURE_INIT || call.interception == FJ_INIT || call.interception == EQE_INIT) {
            capture(result);
        }
    }
}
