package matrix;

import com.datastax.oss.driver.api.core.CqlSession;
import com.datastax.oss.driver.api.core.cql.AsyncResultSet;
import com.datastax.oss.driver.api.core.cql.BatchStatement;
import com.datastax.oss.driver.api.core.cql.DefaultBatchType;
import com.datastax.oss.driver.api.core.cql.PreparedStatement;
import com.datastax.oss.driver.api.core.cql.SimpleStatement;
import java.net.InetSocketAddress;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletionStage;

/**
 * DataStax Java driver 4.x: simple, prepared (also from a request handler), asynchronous and
 * batch statements.
 */
public class Main {
    public static void main(String[] args) throws Exception {
        try (CqlSession session = CqlSession.builder()
                .addContactPoint(new InetSocketAddress("127.0.0.1", 59042))
                .withLocalDatacenter("datacenter1")
                .build()) {
            session.execute("CREATE KEYSPACE IF NOT EXISTS matrix WITH replication = "
                + "{'class': 'SimpleStrategy', 'replication_factor': 1}");
            session.execute("CREATE TABLE IF NOT EXISTS matrix.orders (id int PRIMARY KEY, owner int, total int)");
            session.execute("TRUNCATE matrix.orders");
            PreparedStatement insert = session.prepare("INSERT INTO matrix.orders (id, owner, total) VALUES (?, ?, ?)");
            for (int i = 0; i < 20; i++) {
                session.execute(insert.bind(i, i % 5, i * 10));
            }
            PreparedStatement byId = session.prepare("SELECT * FROM matrix.orders WHERE id = ?");
            Requests.serve(10, id -> String.valueOf(session.execute(byId.bind(Integer.parseInt(id))).one()));
            List<CompletionStage<AsyncResultSet>> pending = new ArrayList<>();
            for (int i = 0; i < 10; i++) {
                pending.add(session.executeAsync(SimpleStatement.newInstance(
                    "UPDATE matrix.orders SET total = ? WHERE id = ?", i, i)));
            }
            for (CompletionStage<AsyncResultSet> p : pending) {
                p.toCompletableFuture().get();
            }
            session.execute(BatchStatement.newInstance(DefaultBatchType.LOGGED)
                .add(insert.bind(100, 1, 1)).add(insert.bind(101, 1, 1)));
            long n = session.execute("SELECT count(*) FROM matrix.orders").one().getLong(0);
            System.out.println("orders " + n);
            if (n != 22) {
                throw new IllegalStateException("orders " + n);
            }
        }
    }
}
