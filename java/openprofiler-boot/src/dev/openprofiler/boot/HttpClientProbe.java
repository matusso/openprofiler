package dev.openprofiler.boot;

/**
 * Outgoing HTTP requests: the JDK {@code HttpClient} and {@code HttpURLConnection}, OkHttp 3/4 and
 * Apache HttpClient 4/5 (classic). The payload is the method and URL without the query.
 */
final class HttpClientProbe extends Probe {
    private static final int JDK_SEND = 0, JDK_SEND_ASYNC = 1, URL_CONNECT = 2, URL_INPUT = 3, OKHTTP4 = 4,
        OKHTTP3 = 5, APACHE4 = 6, APACHE5 = 7;

    HttpClientProbe() {
        super("http_client", "HTTP client", "URL", null);
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.method("jdk.internal.net.http.HttpClientImpl", "send", null).withArgs(),
            Interception.method("jdk.internal.net.http.HttpClientImpl", "sendAsync", null).withArgs(),
            Interception.method("sun.net.www.protocol.http.HttpURLConnection", "connect", "()V"),
            Interception.method("sun.net.www.protocol.http.HttpURLConnection", "getInputStream", "()Ljava/io/InputStream;"),
            Interception.method("okhttp3.internal.connection.RealCall", "execute", "()Lokhttp3/Response;"),
            Interception.method("okhttp3.RealCall", "execute", "()Lokhttp3/Response;"),
            Interception.subtypes("org.apache.http.impl.client.CloseableHttpClient", "doExecute", null).withArgs(),
            Interception.subtypes("org.apache.hc.client5.http.impl.classic.CloseableHttpClient", "doExecute", null).withArgs(),
        };
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        if (call.nested) {
            return;
        }
        String method = null;
        String url = null;
        switch (call.interception) {
            case JDK_SEND:
            case JDK_SEND_ASYNC: {
                Object req = args != null && args.length > 0 ? args[0] : null;
                method = Reflect.str(req, "method");
                url = Reflect.str(req, "uri");
                break;
            }
            case URL_CONNECT:
            case URL_INPUT:
                method = Reflect.str(self, "getRequestMethod");
                url = Reflect.str(self, "getURL");
                break;
            case OKHTTP4:
            case OKHTTP3: {
                Object req = Reflect.call(self, "request");
                method = Reflect.str(req, "method");
                url = Reflect.str(req, "url");
                break;
            }
            case APACHE4: {
                Object host = args != null && args.length > 0 ? args[0] : null;
                Object req = args != null && args.length > 1 ? args[1] : null;
                Object line = Reflect.call(req, "getRequestLine");
                method = Reflect.str(line, "getMethod");
                String uri = Reflect.str(line, "getUri");
                url = uri != null && uri.startsWith("/") && host != null ? Reflect.str(host, "toURI") + uri : uri;
                break;
            }
            case APACHE5: {
                Object host = args != null && args.length > 0 ? args[0] : null;
                Object req = args != null && args.length > 1 ? args[1] : null;
                method = Reflect.str(req, "getMethod");
                Object uri = Reflect.call(req, "getUri");
                url = uri != null ? uri.toString() : Reflect.str(req, "getRequestUri");
                // A relative request URI with the target host passed separately.
                if (url != null && url.startsWith("/") && host != null) {
                    url = Reflect.str(host, "toURI") + url;
                }
                break;
            }
            default:
                break;
        }
        if (method != null && url != null) {
            call.payload = method + " " + normalize(url) + (call.interception == JDK_SEND_ASYNC ? " (async)" : "");
        }
    }

    /** Scheme and host as they are, the path with ids replaced, no query. */
    static String normalize(String url) {
        int scheme = url.indexOf("://");
        int path = url.indexOf('/', scheme < 0 ? 0 : scheme + 3);
        if (path < 0) {
            return Reflect.cut(url, 500);
        }
        return Reflect.cut(url.substring(0, path) + HttpServerProbe.normalize(url.substring(path)), 500);
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (call.payload == null || result == null) {
            return;
        }
        Object status;
        switch (call.interception) {
            case JDK_SEND:
                status = Reflect.call(result, "statusCode");
                break;
            case OKHTTP4:
            case OKHTTP3:
                status = Reflect.call(result, "code");
                break;
            case APACHE4:
                status = Reflect.call(Reflect.call(result, "getStatusLine"), "getStatusCode");
                break;
            case APACHE5:
                status = Reflect.call(result, "getCode");
                break;
            default:
                status = null;
                break;
        }
        if (status instanceof Integer) {
            call.detail = "status " + status;
        }
    }
}
