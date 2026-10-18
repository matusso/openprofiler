package matrix;

import com.datastax.driver.core.BatchStatement;
import com.datastax.driver.core.Cluster;
import com.datastax.driver.core.PreparedStatement;
import com.datastax.driver.core.ResultSetFuture;
import com.datastax.driver.core.Session;
import com.datastax.driver.core.SimpleStatement;
import java.util.ArrayList;
import java.util.List;

/**
 * DataStax Java driver 3.x: simple, prepared (also from a request handler), asynchronous and
 * batch statements.
 */
public class Main {
    public static void main(String[] args) throws Exception {
        try (Cluster cluster = Cluster.builder().addContactPoint("127.0.0.1").withPort(59042)
                .withoutJMXReporting().build();
             Session session = cluster.connect()) {
            session.execute("CREATE KEYSPACE IF NOT EXISTS matrix WITH replication = "
                + "{'class': 'SimpleStrategy', 'replication_factor': 1}");
            session.execute("CREATE TABLE IF NOT EXISTS matrix.orders3 (id int PRIMARY KEY, owner int, total int)");
            session.execute("TRUNCATE matrix.orders3");
            PreparedStatement insert = session.prepare("INSERT INTO matrix.orders3 (id, owner, total) VALUES (?, ?, ?)");
            for (int i = 0; i < 20; i++) {
                session.execute(insert.bind(i, i % 5, i * 10));
            }
            PreparedStatement byId = session.prepare("SELECT * FROM matrix.orders3 WHERE id = ?");
            Requests.serve(10, id -> String.valueOf(session.execute(byId.bind(Integer.parseInt(id))).one()));
            List<ResultSetFuture> pending = new ArrayList<>();
            for (int i = 0; i < 10; i++) {
                pending.add(session.executeAsync(new SimpleStatement(
                    "UPDATE matrix.orders3 SET total = ? WHERE id = ?", i, i)));
            }
            for (ResultSetFuture p : pending) {
                p.getUninterruptibly();
            }
            BatchStatement batch = new BatchStatement();
            batch.add(insert.bind(100, 1, 1));
            batch.add(insert.bind(101, 1, 1));
            session.execute(batch);
            long n = session.execute("SELECT count(*) FROM matrix.orders3").one().getLong(0);
            System.out.println("orders " + n);
            if (n != 22) {
                throw new IllegalStateException("orders " + n);
            }
        }
    }
}
