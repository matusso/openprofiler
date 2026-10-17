package matrix;

import com.sun.net.httpserver.HttpServer;
import com.zaxxer.hikari.HikariConfig;
import com.zaxxer.hikari.HikariDataSource;
import java.lang.ref.WeakReference;
import java.net.InetSocketAddress;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Statement;

/**
 * A real JDBC driver behind HikariCP: DDL, prepared statements with bind parameters, batches,
 * queries run by a request handler, a pooled connection that is never closed (garbage collected
 * by the application) and a physical connection from {@link DriverManager}.
 */
public class Main {
    static HikariDataSource pool;

    public static void main(String[] args) throws Exception {
        String url = System.getProperty("url"), user = System.getProperty("user", ""), pass = System.getProperty("password", "");
        HikariConfig cfg = new HikariConfig();
        cfg.setJdbcUrl(url);
        cfg.setUsername(user);
        cfg.setPassword(pass);
        cfg.setMaximumPoolSize(4);
        pool = new HikariDataSource(cfg);
        try (Connection c = pool.getConnection(); Statement s = c.createStatement()) {
            s.execute("DROP TABLE IF EXISTS visits");
            s.execute("DROP TABLE IF EXISTS owners");
            s.execute("CREATE TABLE owners (id INT PRIMARY KEY, name VARCHAR(80), city VARCHAR(80))");
            s.execute("CREATE TABLE visits (id INT PRIMARY KEY, owner_id INT, note VARCHAR(200))");
        }
        try (Connection c = pool.getConnection();
             PreparedStatement ps = c.prepareStatement("INSERT INTO owners (id, name, city) VALUES (?, ?, ?)")) {
            for (int i = 1; i <= 20; i++) {
                ps.setInt(1, i);
                ps.setString(2, "owner" + i);
                ps.setString(3, i % 2 == 0 ? "Madison" : "Sun Prairie");
                ps.executeUpdate();
            }
        }
        try (Connection c = pool.getConnection();
             PreparedStatement ps = c.prepareStatement("INSERT INTO visits (id, owner_id, note) VALUES (?, ?, ?)")) {
            for (int i = 1; i <= 50; i++) {
                ps.setInt(1, i);
                ps.setInt(2, 1 + i % 20);
                ps.setString(3, "rabies shot");
                ps.addBatch();
            }
            ps.executeBatch();
        }

        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/owners/", ex -> {
            int id = Integer.parseInt(ex.getRequestURI().getPath().substring("/owners/".length()));
            String name;
            try {
                name = findOwner(id);
            } catch (Exception e) {
                name = e.toString();
            }
            byte[] body = name.getBytes();
            ex.sendResponseHeaders(200, body.length);
            ex.getResponseBody().write(body);
            ex.close();
        });
        server.start();
        String base = "http://127.0.0.1:" + server.getAddress().getPort();
        for (int i = 0; i < 40; i++) {
            Http.get(base + "/owners/" + (1 + i % 20));
        }
        server.stop(0);

        try (Connection c = pool.getConnection(); Statement s = c.createStatement()) {
            s.executeUpdate("UPDATE owners SET city = 'Monona' WHERE id > 15");
            try (ResultSet rs = s.executeQuery("SELECT COUNT(*) FROM visits")) {
                rs.next();
            }
        }
        leakPooledConnection();
        // A physical connection, outside the pool.
        try (Connection c = DriverManager.getConnection(url, user, pass); Statement s = c.createStatement()) {
            s.executeQuery("SELECT city FROM owners WHERE id = 1").close();
        }
        // The probe reports collected control objects when it next opens one.
        try (Connection c = pool.getConnection()) {
            c.isValid(1);
        }
        pool.close();
    }

    static String findOwner(int id) throws Exception {
        try (Connection c = pool.getConnection();
             PreparedStatement ps = c.prepareStatement("SELECT name FROM owners WHERE id = ?")) {
            ps.setInt(1, id);
            try (ResultSet rs = ps.executeQuery()) {
                return rs.next() ? rs.getString(1) : "?";
            }
        }
    }

    /** Borrows a connection and drops it without closing it, then waits until it is collected. */
    static void leakPooledConnection() throws Exception {
        WeakReference<Connection> ref = leak();
        for (int i = 0; i < 50 && ref.get() != null; i++) {
            System.gc();
            Thread.sleep(20);
        }
        System.out.println("leaked connection collected: " + (ref.get() == null));
    }

    static WeakReference<Connection> leak() throws Exception {
        Connection c = pool.getConnection();
        c.createStatement().executeQuery("SELECT id FROM owners WHERE name = 'leak'").close();
        return new WeakReference<>(c);
    }
}
