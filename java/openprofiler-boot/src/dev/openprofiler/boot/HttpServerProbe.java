package dev.openprofiler.boot;

import java.util.ArrayList;
import java.util.List;

/**
 * Incoming HTTP requests: Servlet containers (javax and Jakarta; the outermost filter chain or
 * servlet call), the JDK's {@code com.sun.net.httpserver}, and Netty handlers receiving HTTP
 * requests. The payload is the method and the URL path with ids replaced ({@code /owners/{id}}),
 * which also becomes the thread's request label, so call trees can be split by request.
 */
final class HttpServerProbe extends Probe {
    private static final int SERVLET_EXCHANGE = 0, NETTY = 4, JDK = 5;

    HttpServerProbe() {
        super("http_server", "HTTP server", "URL", null);
    }

    @Override
    protected Interception[] interceptions() {
        List<Interception> l = new ArrayList<>();
        for (String pkg : new String[] {"javax.servlet.", "jakarta.servlet."}) {
            l.add(Interception.subtypes(pkg + "FilterChain", "doFilter", null).withArgs());
            l.add(Interception.method(pkg + "http.HttpServlet", "service",
                "(L" + pkg.replace('.', '/') + "ServletRequest;L" + pkg.replace('.', '/') + "ServletResponse;)V").withArgs());
        }
        l.add(Interception.subtypes("io.netty.channel.SimpleChannelInboundHandler", "channelRead", null).withArgs());
        // The filter chain also runs handlers that are lambdas (hidden classes can't be instrumented).
        l.add(Interception.method("com.sun.net.httpserver.Filter$Chain", "doFilter", null).withArgs());
        return l.toArray(new Interception[0]);
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        if (call.nested || args == null || args.length < 1) {
            return;
        }
        String method;
        String path;
        if (call.interception < NETTY) {
            method = Reflect.str(args[0], "getMethod");
            path = Reflect.str(args[0], "getRequestURI");
            call.state = args.length > 1 ? args[1] : null;
        } else if (call.interception == NETTY) {
            Object msg = args.length > 1 ? args[1] : null;
            if (!Reflect.isA(msg, "io.netty.handler.codec.http.HttpRequest")) {
                return;
            }
            method = Reflect.str(msg, "method");
            path = Reflect.str(msg, "uri");
        } else {
            method = Reflect.str(args[0], "getRequestMethod");
            Object uri = Reflect.call(args[0], "getRequestURI");
            path = uri == null ? null : Reflect.str(uri, "getPath");
            call.state = args[0];
        }
        if (method == null || path == null) {
            return;
        }
        String label = method + " " + normalize(path);
        call.payload = label;
        label(call, label);
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (call.payload == null || call.state == null) {
            return;
        }
        Object status = call.interception == JDK ? Reflect.call(call.state, "getResponseCode") : Reflect.call(call.state, "getStatus");
        if (status instanceof Integer) {
            int s = (Integer) status;
            call.detail = "status " + s;
            if (s >= 500 && call.error == null) {
                call.error = "HTTP " + s;
            }
        }
    }

    /** Strips the query and replaces numeric and UUID-like path segments with {@code {id}}. */
    static String normalize(String path) {
        int q = path.indexOf('?');
        if (q >= 0) {
            path = path.substring(0, q);
        }
        StringBuilder sb = new StringBuilder(path.length());
        int i = 0;
        while (i < path.length()) {
            int j = path.indexOf('/', i);
            if (j < 0) {
                j = path.length();
            }
            String seg = path.substring(i, j);
            sb.append(isId(seg) ? "{id}" : seg);
            if (j < path.length()) {
                sb.append('/');
            }
            i = j + 1;
        }
        return Reflect.cut(sb.toString(), 300);
    }

    private static boolean isId(String seg) {
        if (seg.isEmpty()) {
            return false;
        }
        int digits = 0, hex = 0;
        for (int i = 0; i < seg.length(); i++) {
            char c = seg.charAt(i);
            if (c >= '0' && c <= '9') {
                digits++;
            } else if ((c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F') || c == '-') {
                hex++;
            } else {
                return false;
            }
        }
        return digits == seg.length() || (seg.length() >= 16 && digits > 0 && digits + hex == seg.length());
    }
}
