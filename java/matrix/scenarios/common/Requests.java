package matrix;

import com.sun.net.httpserver.HttpServer;
import java.net.InetSocketAddress;
import java.util.function.Function;

/** A JDK HTTP server whose {@code /owners/<id>} requests run {@code handler}: the request label's source. */
final class Requests {
    private Requests() {}

    static void serve(int requests, Function<String, String> handler) throws Exception {
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/owners/", ex -> {
            byte[] body = handler.apply(ex.getRequestURI().getPath().substring("/owners/".length())).getBytes();
            ex.sendResponseHeaders(200, body.length);
            ex.getResponseBody().write(body);
            ex.close();
        });
        server.start();
        String base = "http://127.0.0.1:" + server.getAddress().getPort();
        for (int i = 0; i < requests; i++) {
            Http.get(base + "/owners/" + (1 + i % 5));
        }
        server.stop(0);
    }
}
