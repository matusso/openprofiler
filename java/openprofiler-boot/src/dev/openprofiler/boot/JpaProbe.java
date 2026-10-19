package dev.openprofiler.boot;

import java.util.ArrayList;
import java.util.List;

/**
 * JPA (javax and Jakarta, so Hibernate, EclipseLink, ...): entity operations and query executions.
 * The payload of a query is its JPQL (Hibernate's {@code getQueryString}, EclipseLink's
 * {@code getDatabaseQuery().getJPQLString()}).
 */
final class JpaProbe extends Probe {
    private static final String[] OPS = {"persist", "merge", "remove", "find", "refresh", "flush"};
    private static final String[] QUERIES = {"getResultList", "getSingleResult", "executeUpdate", "getResultStream"};
    /** Payloads by entity class and operation ({@code find Fruit}), so a call builds no string. */
    private static final ClassValue<String[]> PAYLOADS = new ClassValue<String[]>() {
        @Override
        protected String[] computeValue(Class<?> type) {
            return new String[OPS.length];
        }
    };

    JpaProbe() {
        super("jpa", "JPA / Hibernate", "Operation", null);
    }

    @Override
    protected Interception[] interceptions() {
        List<Interception> l = new ArrayList<>();
        for (String pkg : new String[] {"jakarta.persistence.", "javax.persistence."}) {
            for (String op : OPS) {
                l.add(Interception.subtypes(pkg + "EntityManager", op, null).withArgs());
            }
            for (String q : QUERIES) {
                l.add(Interception.subtypes(pkg + "Query", q, null));
            }
        }
        return l.toArray(new Interception[0]);
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        int i = call.interception % (OPS.length + QUERIES.length);
        if (i < OPS.length) {
            String op = OPS[i];
            Object a = args != null && args.length > 0 ? args[0] : null;
            if (a == null) {
                call.payload = op;
                return;
            }
            Class<?> type = a instanceof Class ? (Class<?>) a : a.getClass();
            String[] payloads = PAYLOADS.get(type);
            String payload = payloads[i];
            if (payload == null) {
                String entity = type.getSimpleName();
                payload = payloads[i] = entity.isEmpty() ? op : op + " " + entity;
            }
            call.payload = payload;
        } else {
            String q = Reflect.str(self, "getQueryString");
            if (q == null) {
                // EclipseLink
                q = Reflect.str(Reflect.call(self, "getDatabaseQuery"), "getJPQLString");
            }
            call.payload = q != null ? Reflect.cut(q, 4000) : QUERIES[i - OPS.length] + " " + self.getClass().getSimpleName();
        }
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (result instanceof List) {
            call.detail = ((List<?>) result).size() + " rows";
        } else if (result instanceof Integer) {
            call.detail = result + " rows";
        }
    }
}
