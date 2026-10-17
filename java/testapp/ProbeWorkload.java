import com.sun.net.httpserver.HttpServer;
import dev.openprofiler.api.OpenProfiler;
import dev.openprofiler.api.Probe;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.lang.reflect.Proxy;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.sql.Connection;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Statement;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;

/**
 * Exercises OpenProfiler's probes without third-party libraries: a JDBC "driver" made of
 * {@link Proxy} instances, the JDK HTTP server and client, an executor hand-off from a request
 * handler, exceptions, files, a custom {@link Probe} and plain methods for call counting.
 *
 * <p>Prints {@code PID <pid>} first, then loops for the number of seconds given as the argument.
 */
public class ProbeWorkload {
    static final ExecutorService POOL = Executors.newFixedThreadPool(2, r -> {
        Thread t = new Thread(r, "pw-worker");
        t.setDaemon(true);
        return t;
    });
    static volatile long sink;

    /** A JDBC connection whose statements sleep a little and return nothing. */
    static Connection connection() {
        return (Connection) Proxy.newProxyInstance(ProbeWorkload.class.getClassLoader(),
            new Class<?>[] {Connection.class}, (proxy, m, args) -> {
                switch (m.getName()) {
                    case "prepareStatement":
                        return statement();
                    case "createStatement":
                        return statement();
                    case "close":
                    case "hashCode":
                        return m.getName().equals("hashCode") ? System.identityHashCode(proxy) : null;
                    case "equals":
                        return proxy == args[0];
                    default:
                        return null;
                }
            });
    }

    static PreparedStatement statement() {
        return (PreparedStatement) Proxy.newProxyInstance(ProbeWorkload.class.getClassLoader(),
            new Class<?>[] {PreparedStatement.class}, (proxy, m, args) -> {
                switch (m.getName()) {
                    case "executeQuery":
                        Thread.sleep(2);
                        return null;
                    case "executeUpdate":
                        Thread.sleep(1);
                        return 1;
                    case "execute":
                        return false;
                    case "hashCode":
                        return System.identityHashCode(proxy);
                    case "equals":
                        return proxy == args[0];
                    default:
                        return null;
                }
            });
    }

    static void queryOrder(int id) throws Exception {
        Connection c = connection();
        try (PreparedStatement ps = c.prepareStatement("SELECT * FROM orders WHERE id = ?")) {
            ps.setInt(1, id);
            ResultSet rs = ps.executeQuery();
            sink += rs == null ? 1 : 2;
        }
        try (Statement st = c.createStatement()) {
            st.executeUpdate("UPDATE stats SET hits = hits + 1");
        }
    }

    @Probe(value = "orders", payload = "price order {0}")
    static long priceOrder(int id) {
        return work(id);
    }

    static long work(int n) {
        long x = 0;
        for (int i = 0; i < 20_000; i++) {
            x += leaf(i ^ n);
        }
        return x;
    }

    static long leaf(int i) {
        return (i * 31L) % 7;
    }

    /** Insertion sort, for complexity analysis (time grows with the square of the length). */
    static int[] sortItems(int[] items) {
        for (int i = 1; i < items.length; i++) {
            int v = items[i];
            int j = i - 1;
            while (j >= 0 && items[j] > v) {
                items[j + 1] = items[j];
                j--;
            }
            items[j + 1] = v;
        }
        return items;
    }

    static void touchFile(File f) throws Exception {
        try (FileOutputStream out = new FileOutputStream(f, false)) {
            out.write("hello probes".getBytes(StandardCharsets.UTF_8), 0, 12);
        }
        try (FileInputStream in = new FileInputStream(f)) {
            sink += in.read(new byte[64], 0, 64);
        }
    }

    public static void main(String[] args) throws Exception {
        long seconds = args.length > 0 ? Long.parseLong(args[0]) : 30;
        System.out.println("PID " + ProcessHandle.current().pid());
        System.out.flush();
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/orders/", ex -> {
            int id = Integer.parseInt(ex.getRequestURI().getPath().substring("/orders/".length()));
            try {
                // Work handed to another thread is attributed to this request.
                Future<?> f = POOL.submit(() -> {
                    queryOrder(id);
                    return null;
                });
                f.get();
                sink += priceOrder(id);
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
            byte[] body = ("order " + id).getBytes(StandardCharsets.UTF_8);
            ex.sendResponseHeaders(id % 10 == 9 ? 500 : 200, body.length);
            ex.getResponseBody().write(body);
            ex.close();
        });
        server.start();
        int port = server.getAddress().getPort();
        HttpClient client = HttpClient.newHttpClient();
        File tmp = File.createTempFile("probe-workload", ".txt");
        tmp.deleteOnExit();
        long end = System.nanoTime() + seconds * 1_000_000_000L;
        int i = 0;
        while (System.nanoTime() < end) {
            i++;
            HttpRequest req = HttpRequest.newBuilder(URI.create("http://127.0.0.1:" + port + "/orders/" + i)).build();
            client.send(req, HttpResponse.BodyHandlers.ofString());
            try {
                Integer.parseInt("not a number " + i);
            } catch (NumberFormatException expected) {
                sink++;
            }
            touchFile(tmp);
            OpenProfiler.probeEvent("manual", "tick", 1_000_000);
            int[] items = new int[100 + (i % 20) * 100];
            for (int k = 0; k < items.length; k++) {
                items[k] = items.length - k;
            }
            sink += sortItems(items)[0];
            Thread.sleep(20);
        }
        server.stop(0);
        System.out.println("done " + sink);
    }
}
