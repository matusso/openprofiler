package matrix;

import com.sun.net.httpserver.HttpServer;
import org.apache.http.HttpHost;
import org.apache.http.client.methods.CloseableHttpResponse;
import org.apache.http.client.methods.HttpGet;
import org.apache.http.client.methods.HttpPost;
import org.apache.http.entity.StringEntity;
import org.apache.http.impl.client.CloseableHttpClient;
import org.apache.http.impl.client.HttpClients;
import org.apache.http.util.EntityUtils;

/** Apache HttpClient 4: absolute URIs, and a request with a target host and a relative URI. */
public class Main {
    static final CloseableHttpClient CLIENT = HttpClients.createDefault();

    public static void main(String[] args) throws Exception {
        HttpServer server = Backend.start();
        String base = Backend.base(server);
        for (int i = 0; i < 30; i++) {
            fetch(base + "/api/items/" + (100 + i) + "?q=" + i);
        }
        HttpHost host = new HttpHost("127.0.0.1", server.getAddress().getPort(), "http");
        for (int i = 0; i < 10; i++) {
            HttpPost post = new HttpPost("/api/items");
            post.setEntity(new StringEntity("{}"));
            try (CloseableHttpResponse r = CLIENT.execute(host, post)) {
                EntityUtils.consume(r.getEntity());
            }
        }
        fetch(base + "/api/missing");
        CLIENT.close();
        server.stop(0);
    }

    static int fetch(String url) throws Exception {
        try (CloseableHttpResponse r = CLIENT.execute(new HttpGet(url))) {
            EntityUtils.consume(r.getEntity());
            return r.getStatusLine().getStatusCode();
        }
    }
}
