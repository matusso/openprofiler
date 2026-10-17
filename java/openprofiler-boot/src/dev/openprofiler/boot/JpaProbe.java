package dev.openprofiler.boot;

import java.util.ArrayList;
import java.util.List;

/** JPA (javax and Jakarta, so Hibernate, EclipseLink, ...): entity operations and query executions. */
final class JpaProbe extends Probe {
    private static final String[] OPS = {"persist", "merge", "remove", "find", "refresh", "flush"};
    private static final String[] QUERIES = {"getResultList", "getSingleResult", "executeUpdate", "getResultStream"};

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
            String entity = a == null ? "" : a instanceof Class ? ((Class<?>) a).getSimpleName() : a.getClass().getSimpleName();
            call.payload = entity.isEmpty() ? op : op + " " + entity;
        } else {
            String q = Reflect.str(self, "getQueryString");
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
