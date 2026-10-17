package dev.openprofiler.boot;

import java.util.ArrayList;
import java.util.List;

/**
 * JDBC: statements with their SQL (prepared statements remember the SQL they were prepared with),
 * bind parameters as event detail, and connections as control objects. Physical connections come
 * from {@code Driver.connect}; logical ones from {@code DataSource.getConnection} (connection
 * pools), so a pooled connection that is never closed shows up as open or as garbage collected
 * without close.
 */
final class JdbcProbe extends Probe {
    private static final int PREPARE = 0, PREPARE_CALL = 1, EXECUTE = 2, EXECUTE_QUERY = 3, EXECUTE_UPDATE = 4,
        EXECUTE_LARGE_UPDATE = 5, EXECUTE_BATCH = 6, EXECUTE_LARGE_BATCH = 7, ADD_BATCH = 8, CONNECT = 9,
        GET_CONNECTION = 10, CLOSE = 11, CLEAR_PARAMETERS = 12, STATEMENT_CLOSE = 13, FIRST_SETTER = 14;
    private static final String[] SETTERS = {
        "setString", "setInt", "setLong", "setObject", "setBoolean", "setDouble", "setFloat", "setShort",
        "setBigDecimal", "setTimestamp", "setDate", "setTime", "setNull",
    };
    private static final int MAX_PARAMS = 32;

    private final WeakIdentityMap<Object, String> sql = new WeakIdentityMap<>();
    private final WeakIdentityMap<Object, Object[]> params = new WeakIdentityMap<>();
    private final WeakIdentityMap<Object, Long> connectionOf = new WeakIdentityMap<>();
    /** Data source → the URL its connections report (looked up once). */
    private final WeakIdentityMap<Object, String> dataSourceUrl = new WeakIdentityMap<>();

    JdbcProbe() {
        super("jdbc", "JDBC", "SQL statement", "Connection");
    }

    @Override
    protected Interception[] interceptions() {
        List<Interception> l = new ArrayList<>();
        l.add(Interception.subtypes("java.sql.Connection", "prepareStatement", null).withArgs());
        l.add(Interception.subtypes("java.sql.Connection", "prepareCall", null).withArgs());
        for (String m : new String[] {"execute", "executeQuery", "executeUpdate", "executeLargeUpdate"}) {
            l.add(Interception.subtypes("java.sql.Statement", m, null).withArgs());
        }
        l.add(Interception.subtypes("java.sql.Statement", "executeBatch", "()[I"));
        l.add(Interception.subtypes("java.sql.Statement", "executeLargeBatch", "()[J"));
        l.add(Interception.subtypes("java.sql.Statement", "addBatch", "(Ljava/lang/String;)V").withArgs().entryOnly());
        l.add(Interception.subtypes("java.sql.Driver", "connect", null).withArgs());
        l.add(Interception.subtypes("javax.sql.DataSource", "getConnection", null));
        l.add(Interception.subtypes("java.sql.Connection", "close", "()V").entryOnly());
        l.add(Interception.subtypes("java.sql.PreparedStatement", "clearParameters", "()V").entryOnly());
        // Dropping a closed statement's entries spares the GC clearing their weak references.
        l.add(Interception.subtypes("java.sql.Statement", "close", "()V").entryOnly());
        for (String s : SETTERS) {
            l.add(Interception.subtypes("java.sql.PreparedStatement", s, null).withArgs().entryOnly());
        }
        return l.toArray(new Interception[0]);
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        int i = call.interception;
        if (i >= FIRST_SETTER) {
            if (args != null && args.length >= 2 && args[0] instanceof Integer) {
                bind(self, (Integer) args[0], i == FIRST_SETTER + SETTERS.length - 1 ? null : args[1]);
            }
            return;
        }
        switch (i) {
            case PREPARE:
            case PREPARE_CALL:
                call.state = args != null && args.length > 0 && args[0] instanceof String ? args[0] : null;
                break;
            case EXECUTE:
            case EXECUTE_QUERY:
            case EXECUTE_UPDATE:
            case EXECUTE_LARGE_UPDATE: {
                String text = args != null && args.length > 0 && args[0] instanceof String ? (String) args[0] : sql.get(self);
                call.payload = text == null ? "[unknown statement]" : Reflect.cut(text, 4000);
                call.detail = paramsText(self);
                call.control = connectionId(self);
                break;
            }
            case EXECUTE_BATCH:
            case EXECUTE_LARGE_BATCH: {
                String text = sql.get(self);
                call.payload = "[batch] " + (text == null ? "statements" : Reflect.cut(text, 4000));
                call.control = connectionId(self);
                break;
            }
            case ADD_BATCH:
                if (args != null && args.length > 0 && args[0] instanceof String) {
                    sql.put(self, (String) args[0]);
                }
                break;
            case CONNECT:
                call.state = args != null && args.length > 0 ? args[0] : null;
                call.payload = "[connect]";
                break;
            case GET_CONNECTION:
                call.payload = "[get connection]";
                break;
            case CLOSE:
                closeControl(self);
                break;
            case CLEAR_PARAMETERS:
                params.remove(self);
                break;
            case STATEMENT_CLOSE:
                sql.remove(self);
                params.remove(self);
                connectionOf.remove(self);
                break;
            default:
                break;
        }
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        switch (call.interception) {
            case PREPARE:
            case PREPARE_CALL:
                if (result != null && call.state != null) {
                    sql.put(result, (String) call.state);
                    long conn = controlId(self);
                    if (conn != 0) {
                        connectionOf.put(result, conn);
                    }
                }
                break;
            case CONNECT:
                if (result != null) {
                    call.control = openControl(result, call.state + " (physical)");
                }
                break;
            case GET_CONNECTION:
                if (result != null) {
                    String url = dataSourceUrl.get(self);
                    if (url == null) {
                        Object meta = Reflect.call(result, "getMetaData");
                        url = meta == null ? null : Reflect.str(meta, "getURL");
                        url = url != null ? url : self.getClass().getName();
                        dataSourceUrl.put(self, url);
                    }
                    call.control = openControl(result, url + " (pooled)");
                }
                break;
            default:
                break;
        }
    }

    private void bind(Object stmt, int index, Object value) {
        if (index < 1 || index > MAX_PARAMS) {
            return;
        }
        Object[] p = params.get(stmt);
        if (p == null || p.length < index) {
            Object[] n = new Object[Math.max(index, p == null ? 4 : p.length * 2)];
            if (p != null) {
                System.arraycopy(p, 0, n, 0, p.length);
            }
            p = n;
            params.put(stmt, p);
        }
        p[index - 1] = value == null ? "NULL" : value;
    }

    private String paramsText(Object stmt) {
        Object[] p = params.get(stmt);
        if (p == null) {
            return null;
        }
        StringBuilder sb = new StringBuilder("params: ");
        int last = p.length;
        while (last > 0 && p[last - 1] == null) {
            last--;
        }
        for (int i = 0; i < last; i++) {
            if (i > 0) {
                sb.append(", ");
            }
            Object v = p[i];
            sb.append(v instanceof String && !"NULL".equals(v) ? "'" + Reflect.cut((String) v, 100) + "'" : String.valueOf(v));
        }
        return sb.toString();
    }

    private long connectionId(Object stmt) {
        Long c = connectionOf.get(stmt);
        if (c != null) {
            return c;
        }
        return controlId(Reflect.call(stmt, "getConnection"));
    }
}
