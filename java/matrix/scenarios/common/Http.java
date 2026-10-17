package matrix;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;

/** Requests to the scenario's own server (the JDK client is probed too, which doesn't matter). */
final class Http {
    private static final HttpClient CLIENT = HttpClient.newBuilder().connectTimeout(Duration.ofSeconds(5)).build();
    /** Without the {@code h2c} upgrade the default client asks for. */
    private static final HttpClient HTTP1 =
        HttpClient.newBuilder().version(HttpClient.Version.HTTP_1_1).connectTimeout(Duration.ofSeconds(5)).build();

    private Http() {}

    static int get(String url) throws Exception {
        HttpRequest req = HttpRequest.newBuilder(URI.create(url)).timeout(Duration.ofSeconds(20)).build();
        return CLIENT.send(req, HttpResponse.BodyHandlers.discarding()).statusCode();
    }

    static int get1(String url) throws Exception {
        HttpRequest req = HttpRequest.newBuilder(URI.create(url)).timeout(Duration.ofSeconds(20)).build();
        return HTTP1.send(req, HttpResponse.BodyHandlers.discarding()).statusCode();
    }

    /** Waits until {@code url} answers at all. */
    static void await(String url) throws Exception {
        for (int i = 0; ; i++) {
            try {
                get(url);
                return;
            } catch (Exception e) {
                if (i > 100) {
                    throw e;
                }
                Thread.sleep(100);
            }
        }
    }
}
