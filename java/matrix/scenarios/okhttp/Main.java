package matrix;

import com.sun.net.httpserver.HttpServer;
import okhttp3.MediaType;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.RequestBody;
import okhttp3.Response;

/** OkHttp synchronous calls. */
public class Main {
    static final OkHttpClient CLIENT = new OkHttpClient();

    public static void main(String[] args) throws Exception {
        HttpServer server = Backend.start();
        String base = Backend.base(server);
        for (int i = 0; i < 30; i++) {
            fetch(base + "/api/items/" + (100 + i) + "?q=" + i);
        }
        for (int i = 0; i < 10; i++) {
            Request req = new Request.Builder().url(base + "/api/items")
                .post(RequestBody.create(MediaType.parse("application/json"), "{}")).build();
            try (Response r = CLIENT.newCall(req).execute()) {
                r.body().string();
            }
        }
        fetch(base + "/api/missing");
        server.stop(0);
        CLIENT.dispatcher().executorService().shutdown();
        CLIENT.connectionPool().evictAll();
    }

    static int fetch(String url) throws Exception {
        try (Response r = CLIENT.newCall(new Request.Builder().url(url).build()).execute()) {
            r.body().string();
            return r.code();
        }
    }
}
