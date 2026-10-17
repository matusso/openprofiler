package dev.openprofiler.boot;

import java.util.Iterator;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

/**
 * MongoDB Java driver (sync, and the reactive streams and async drivers): every command a
 * connection sends, by command and collection ({@code find matrix.orders}). Synchronous commands
 * are timed by {@code InternalStreamConnection.sendAndReceive}; asynchronous ones from
 * {@code sendAndReceiveAsync} until their reply is decoded ({@code getCommandResult}, matched by
 * message id), wherever that happens. Handshakes and heartbeats are not recorded.
 */
final class MongoProbe extends Probe {
    private static final int SEND = 0, SEND_ASYNC = 1, RESULT = 2;
    private static final String CONNECTION = "com.mongodb.internal.connection.InternalStreamConnection";
    /** Asynchronous commands sent and not answered yet, by message id. */
    private final ConcurrentHashMap<Integer, Pending> pending = new ConcurrentHashMap<>();

    MongoProbe() {
        super("mongodb", "MongoDB", "Command", null);
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.method(CONNECTION, "sendAndReceive", null).withArgs(),
            Interception.method(CONNECTION, "sendAndReceiveAsync", null).withArgs().entryOnly(),
            Interception.method(CONNECTION, "getCommandResult", null).withArgs(),
        };
    }

    private static final class Pending {
        final String payload;
        final String label;
        final long startEpochNanos;

        Pending(String payload, String label, long startEpochNanos) {
            this.payload = payload;
            this.label = label;
            this.startEpochNanos = startEpochNanos;
        }
    }

    /** {@code <command> <database>.<collection>}, or null for handshakes and heartbeats. */
    static String payload(Object message) {
        Object command = Reflect.field(message, "command");
        String name = Reflect.str(command, "getFirstKey");
        if (name == null) {
            return null;
        }
        switch (name) {
            case "hello":
            case "isMaster":
            case "ismaster":
            case "saslStart":
            case "saslContinue":
            case "endSessions":
                return null;
            default:
                break;
        }
        String db = Reflect.str(Reflect.field(message, "namespace"), "getDatabaseName");
        // The collection is the command's value (find, insert, aggregate...), or a field of it.
        Object coll = Reflect.call(Reflect.call1(command, "get", name), "getValue");
        if (!(coll instanceof String)) {
            coll = Reflect.call(Reflect.call1(command, "get", "collection"), "getValue");
        }
        StringBuilder b = new StringBuilder(name);
        if (db != null) {
            b.append(' ').append(db);
            if (coll instanceof String) {
                b.append('.').append(coll);
            }
        }
        return Reflect.cut(b.toString(), 500);
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        Object message = args != null && args.length > 0 ? args[0] : null;
        switch (call.interception) {
            case SEND:
                if (!call.nested) {
                    call.payload = payload(message);
                }
                break;
            case SEND_ASYNC: {
                String payload = payload(message);
                Object id = Reflect.call(message, "getId");
                if (payload != null && id instanceof Integer) {
                    if (pending.size() > 10_000) {
                        expire();
                    }
                    ProbeThread t = ProbeThread.current();
                    pending.put((Integer) id, new Pending(payload, t == null ? null : t.context, Probes.epochNanos()));
                }
                break;
            }
            case RESULT:
                // Synchronous commands are recorded by sendAndReceive.
                if (args != null && !pending.isEmpty()) {
                    for (Object a : args) {
                        if (a instanceof Integer) {
                            call.state = pending.remove(a);
                            break;
                        }
                    }
                }
                break;
            default:
                break;
        }
    }

    /** Drops asynchronous commands that never got a reply (closed connections) after a minute. */
    private void expire() {
        long old = Probes.epochNanos() - 60_000_000_000L;
        for (Iterator<Map.Entry<Integer, Pending>> it = pending.entrySet().iterator(); it.hasNext(); ) {
            if (it.next().getValue().startEpochNanos < old) {
                it.remove();
            }
        }
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (call.interception == RESULT && call.state instanceof Pending) {
            Pending p = (Pending) call.state;
            recordFor(p.label, p.payload, p.startEpochNanos, Probes.epochNanos() - p.startEpochNanos, call.error, null);
        }
    }
}
