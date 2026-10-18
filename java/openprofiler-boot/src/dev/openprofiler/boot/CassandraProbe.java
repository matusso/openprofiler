package dev.openprofiler.boot;

import java.util.concurrent.ExecutionException;
import java.util.concurrent.Executor;
import java.util.concurrent.Future;

/**
 * Cassandra: the DataStax Java driver 4.x ({@code com.datastax.oss}, also the Apache Cassandra
 * driver) and 3.x ({@code com.datastax.driver}). Statements executed synchronously and
 * asynchronously (timed until their future completes), by CQL; batches by their first statement.
 */
final class CassandraProbe extends Probe {
    private static final int V4_EXECUTE = 0, V3_EXECUTE = 1, V3_EXECUTE_ASYNC = 2;
    private static final Executor DIRECT = Runnable::run;

    CassandraProbe() {
        super("cassandra", "Cassandra", "CQL", null);
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.subtypes("com.datastax.oss.driver.api.core.session.Session", "execute",
                                  "(Lcom/datastax/oss/driver/api/core/session/Request;"
                                  + "Lcom/datastax/oss/driver/api/core/type/reflect/GenericType;)Ljava/lang/Object;")
                .withArgs(),
            Interception.method("com.datastax.driver.core.AbstractSession", "execute",
                                "(Lcom/datastax/driver/core/Statement;)Lcom/datastax/driver/core/ResultSet;")
                .withArgs(),
            Interception.subtypes("com.datastax.driver.core.AbstractSession", "executeAsync",
                                  "(Lcom/datastax/driver/core/Statement;)Lcom/datastax/driver/core/ResultSetFuture;")
                .withArgs(),
        };
    }

    /** The CQL of a statement, or null for requests that aren't statements (prepare). */
    static String cql(Object statement) {
        for (int i = 0; i < 4 && statement != null; i++) {
            // 4.x
            if (Reflect.isA(statement, "com.datastax.oss.driver.api.core.cql.SimpleStatement")) {
                return Reflect.str(statement, "getQuery");
            }
            if (Reflect.isA(statement, "com.datastax.oss.driver.api.core.cql.BoundStatement")) {
                return Reflect.str(Reflect.call(statement, "getPreparedStatement"), "getQuery");
            }
            if (Reflect.isA(statement, "com.datastax.oss.driver.api.core.cql.BatchStatement")) {
                return batch(statement, Reflect.call(statement, "iterator"));
            }
            // 3.x
            if (Reflect.isA(statement, "com.datastax.driver.core.RegularStatement")) {
                return Reflect.str(statement, "getQueryString");
            }
            if (Reflect.isA(statement, "com.datastax.driver.core.BoundStatement")) {
                return Reflect.str(Reflect.call(statement, "preparedStatement"), "getQueryString");
            }
            if (Reflect.isA(statement, "com.datastax.driver.core.BatchStatement")) {
                return batch(statement, Reflect.call(Reflect.call(statement, "getStatements"), "iterator"));
            }
            if (Reflect.isA(statement, "com.datastax.driver.core.StatementWrapper")) {
                statement = Reflect.field(statement, "wrapped");
                continue;
            }
            return null;
        }
        return null;
    }

    private static String batch(Object batch, Object statements) {
        Object first = statements instanceof java.util.Iterator && ((java.util.Iterator<?>) statements).hasNext()
            ? ((java.util.Iterator<?>) statements).next() : null;
        String cql = first == null ? null : cql(first);
        Object size = Reflect.call(batch, "size");
        return "BATCH " + size + (cql == null ? "" : ": " + cql);
    }

    private static String rows(Object resultSet) {
        Object n = Reflect.call(resultSet, "getAvailableWithoutFetching");
        if (n == null) {
            n = Reflect.call(resultSet, "remaining");
        }
        return n instanceof Integer ? n + " rows" : null;
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        if (call.nested || args == null || args.length == 0) {
            return;
        }
        String cql = cql(args[0]);
        if (cql != null) {
            call.payload = Reflect.cut(cql, 2000);
        }
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (call.payload == null || error != null) {
            return;
        }
        switch (call.interception) {
            case V4_EXECUTE:
                // executeAsync: the result is a CompletionStage.
                if (!recordOnCompletion(call, result, CassandraProbe::rows)) {
                    call.detail = rows(result);
                }
                break;
            case V3_EXECUTE:
                call.detail = rows(result);
                break;
            case V3_EXECUTE_ASYNC:
                if (result instanceof Future) {
                    whenDone(call, (Future<?>) result);
                }
                break;
            default:
                break;
        }
    }

    /** 3.x futures are Guava {@code ListenableFuture}s: records the call from a listener. */
    private void whenDone(Call call, Future<?> future) {
        String payload = call.payload;
        long startEpochNanos = call.startEpochNanos;
        ProbeThread t = ProbeThread.current();
        String label = t == null ? null : t.context;
        Runnable listener = () -> {
            long duration = Probes.epochNanos() - startEpochNanos;
            String err = null;
            String detail = null;
            try {
                detail = rows(future.get());
            } catch (ExecutionException e) {
                Throwable c = e.getCause() == null ? e : e.getCause();
                err = Reflect.cut(c.getClass().getName() + (c.getMessage() == null ? "" : ": " + c.getMessage()), 500);
            } catch (Throwable e) {
                err = e.getClass().getName();
            }
            recordFor(label, payload, startEpochNanos, duration, err, detail);
        };
        if (Reflect.isA(future, "com.google.common.util.concurrent.ListenableFuture")) {
            Reflect.call2(future, "addListener", listener, DIRECT);
            call.ignore();
        }
    }
}
