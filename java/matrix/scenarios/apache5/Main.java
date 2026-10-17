package matrix;

import com.sun.net.httpserver.HttpServer;
import org.apache.hc.client5.http.classic.methods.HttpGet;
import org.apache.hc.client5.http.classic.methods.HttpPost;
import org.apache.hc.client5.http.impl.classic.CloseableHttpClient;
import org.apache.hc.client5.http.impl.classic.CloseableHttpResponse;
import org.apache.hc.client5.http.impl.classic.HttpClients;
import org.apache.hc.core5.http.ContentType;
import org.apache.hc.core5.http.HttpHost;
import org.apache.hc.core5.http.io.entity.EntityUtils;
import org.apache.hc.core5.http.io.entity.StringEntity;

/** Apache HttpClient 5 classic: absolute URIs, and a request with a target host and a relative URI. */
public class Main {
    static final CloseableHttpClient CLIENT = HttpClients.createDefault();

    public static void main(String[] args) throws Exception {
        HttpServer server = Backend.start();
        String base = Backend.base(server);
        for (int i = 0; i < 30; i++) {
            fetch(base + "/api/items/" + (100 + i) + "?q=" + i);
        }
        HttpHost host = new HttpHost("http", "127.0.0.1", server.getAddress().getPort());
        for (int i = 0; i < 10; i++) {
            HttpPost post = new HttpPost("/api/items");
            post.setEntity(new StringEntity("{}", ContentType.APPLICATION_JSON));
            try (CloseableHttpResponse r = CLIENT.execute(host, post)) {
                EntityUtils.consume(r.getEntity());
            }
        }
        fetch(base + "/api/missing");
        CLIENT.close();
        server.stop(0);
    }

    @SuppressWarnings("deprecation")
    static int fetch(String url) throws Exception {
        try (CloseableHttpResponse r = CLIENT.execute(new HttpGet(url))) {
            EntityUtils.consume(r.getEntity());
            return r.getCode();
        }
    }
}
