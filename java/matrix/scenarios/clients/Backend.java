package matrix;

import com.sun.net.httpserver.HttpServer;
import java.net.InetSocketAddress;

/** The server the HTTP client scenarios call: {@code /api/items/<id>} and {@code /api/missing}. */
final class Backend {
    private Backend() {}

    static HttpServer start() throws Exception {
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/api/", ex -> {
            ex.getRequestBody().readAllBytes();
            boolean missing = ex.getRequestURI().getPath().startsWith("/api/missing");
            byte[] body = "{\"ok\":true}".getBytes();
            ex.sendResponseHeaders(missing ? 404 : 200, body.length);
            ex.getResponseBody().write(body);
            ex.close();
        });
        server.start();
        return server;
    }

    static String base(HttpServer server) {
        return "http://127.0.0.1:" + server.getAddress().getPort();
    }
}
