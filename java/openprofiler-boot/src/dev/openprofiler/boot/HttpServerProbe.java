package dev.openprofiler.boot;

import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.List;

/**
 * Incoming HTTP requests: Servlet containers (javax and Jakarta; the outermost filter chain or
 * servlet call), the JDK's {@code com.sun.net.httpserver}, Netty handlers receiving HTTP requests,
 * Reactor Netty (so Spring WebFlux on its default server) and Vert.x (so Quarkus). The payload is the method and the
 * URL path with ids replaced ({@code /owners/{id}}), which also becomes the thread's request
 * label, so call trees can be split by request.
 *
 * <p>Reactor Netty requests are asynchronous: the request is labelled while its handler is
 * applied and subscribed (work it schedules on Reactor schedulers or executors inherits the
 * label), and recorded when the response completes, with the time from receipt to completion.
 * Vert.x requests work the same way: labelled while the connection (HTTP/1.x) or stream (HTTP/2,
 * also after an {@code h2c} upgrade) dispatches them, recorded when their response is written.
 * Servlet requests put into asynchronous mode ({@code startAsync}, as Spring MVC's async return
 * values and WebFlux on a servlet container do) are recorded when an {@code AsyncListener}
 * reports their completion.
 */
final class HttpServerProbe extends Probe {
    private static final int NETTY = 4, JDK = 5, REACTOR_STATE = 6, REACTOR_COMPLETE = 7, REACTOR_ERROR = 8,
        VERTX_MESSAGE = 9, VERTX_COMPLETE = 10, VERTX2_HEADERS = 11, VERTX2_END = 12;
    private static final String VERTX_CONNECTION = "io.vertx.core.http.impl.Http1xServerConnection";
    private static final String REACTOR_OPS = "reactor.netty.http.server.HttpServerOperations";

    /** Reactor Netty requests in flight: operations → pending request. */
    private final WeakIdentityMap<Object, Pending> pending = new WeakIdentityMap<>();
    /** Vert.x requests in flight: connection → pending requests in order. */
    private final WeakIdentityMap<Object, ArrayDeque<Pending>> vertx = new WeakIdentityMap<>();

    private static final class Pending {
        final String label;
        final long startEpochNanos;
        final long start;

        Pending(String label, long startEpochNanos, long start) {
            this.label = label;
            this.startEpochNanos = startEpochNanos;
            this.start = start;
        }
    }

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
        l.add(Interception.method("reactor.netty.http.server.HttpServer$HttpServerHandle", "onStateChange", null).withArgs());
        l.add(Interception.method(REACTOR_OPS, "onOutboundComplete", "()V").entryOnly());
        l.add(Interception.method(REACTOR_OPS, "onOutboundError", "(Ljava/lang/Throwable;)V").withArgs().entryOnly());
        l.add(Interception.method(VERTX_CONNECTION, "handleMessage", "(Ljava/lang/Object;)V").withArgs());
        l.add(Interception.method(VERTX_CONNECTION, "responseComplete", "()V").entryOnly());
        l.add(Interception.method("io.vertx.core.http.impl.Http2ServerStream", "onHeaders", null));
        // write(ByteBuf, boolean end[, Handler]): every response body write funnels into it.
        l.add(Interception.method("io.vertx.core.http.impl.Http2ServerResponse", "write", null).withArgs().entryOnly());
        return l.toArray(new Interception[0]);
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        if (call.interception == REACTOR_COMPLETE || call.interception == REACTOR_ERROR) {
            reactorDone(self, args);
            return;
        }
        if (call.interception == VERTX_COMPLETE) {
            vertxDone(self);
            return;
        }
        if (call.interception == VERTX2_END) {
            if (args == null || args.length < 2 || !Boolean.TRUE.equals(args[1])) {
                return;
            }
            Pending p = pending.remove(self);
            if (p != null) {
                done(p, Reflect.call(self, "getStatusCode"), null);
            }
            return;
        }
        if (call.interception == VERTX2_HEADERS) {
            if (!call.nested) {
                Object request = Reflect.field(self, "request");
                received(call, Reflect.call(request, "response"), Reflect.str(request, "method"), Reflect.str(request, "uri"));
            }
            return;
        }
        if (call.nested || args == null || args.length < 1) {
            return;
        }
        if (call.interception == REACTOR_STATE) {
            reactorReceived(call, args);
            return;
        }
        if (call.interception == VERTX_MESSAGE) {
            vertxReceived(call, self, args[0]);
            return;
        }
        String method;
        String path;
        if (call.interception < NETTY) {
            method = Reflect.str(args[0], "getMethod");
            path = Reflect.str(args[0], "getRequestURI");
            call.state = args.length > 1 ? new Object[] {args[0], args[1]} : null;
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
        Object status;
        if (call.interception < NETTY) {
            Object[] exchange = (Object[]) call.state;
            if (error == null && Boolean.TRUE.equals(Reflect.call(exchange[0], "isAsyncStarted"))
                    && listenAsync(call, exchange[0], exchange[1])) {
                call.ignore();
                return;
            }
            status = Reflect.call(exchange[1], "getStatus");
        } else {
            status = Reflect.call(call.state, "getResponseCode");
        }
        if (status instanceof Integer) {
            int s = (Integer) status;
            call.detail = "status " + s;
            if (s >= 500 && call.error == null) {
                call.error = "HTTP " + s;
            }
        }
    }

    /** Registers a listener that records an asynchronous servlet request when it completes. */
    private boolean listenAsync(Call call, Object request, Object response) {
        try {
            String pkg = Reflect.isA(request, "jakarta.servlet.ServletRequest") ? "jakarta.servlet." : "javax.servlet.";
            Class<?> type = Class.forName(pkg + "AsyncListener", false, request.getClass().getClassLoader());
            Object context = Reflect.call(request, "getAsyncContext");
            if (context == null) {
                return false;
            }
            AsyncDone done = new AsyncDone(this, call.payload, call.startEpochNanos, call.start, response);
            Object listener = Proxy.newProxyInstance(type.getClassLoader(), new Class<?>[] {type}, done);
            return Reflect.tryCall1(context, "addListener", listener);
        } catch (Throwable t) {
            return false;
        }
    }

    /** Records one request with itself as context (completions run on other threads). */
    void recordFor(String label, long startEpochNanos, long duration, String error, String detail) {
        recordFor(label, label, startEpochNanos, duration, error, detail);
    }

    /** The {@code AsyncListener} of an asynchronous servlet request (a proxy, as servlet APIs vary). */
    static final class AsyncDone implements InvocationHandler {
        private final HttpServerProbe probe;
        private final String label;
        private final long startEpochNanos;
        private final long start;
        private final Object response;
        private boolean done;

        AsyncDone(HttpServerProbe probe, String label, long startEpochNanos, long start, Object response) {
            this.probe = probe;
            this.label = label;
            this.startEpochNanos = startEpochNanos;
            this.start = start;
            this.response = response;
        }

        @Override
        public Object invoke(Object proxy, Method m, Object[] args) {
            switch (m.getName()) {
                case "hashCode":
                    return System.identityHashCode(proxy);
                case "equals":
                    return proxy == args[0];
                case "toString":
                    return "openprofiler async listener";
                default:
                    break;
            }
            ProbeThread t = ProbeThread.current();
            if (t == null || t.busy) {
                return null;
            }
            t.busy = true;
            try {
                Object event = args != null && args.length > 0 ? args[0] : null;
                switch (m.getName()) {
                    case "onComplete":
                        finish(null);
                        break;
                    case "onTimeout":
                        finish("timeout");
                        break;
                    case "onError": {
                        Object e = Reflect.call(event, "getThrowable");
                        finish(e instanceof Throwable
                            ? e.getClass().getName() + (((Throwable) e).getMessage() == null ? "" : ": " + ((Throwable) e).getMessage())
                            : "error");
                        break;
                    }
                    case "onStartAsync":
                        // Another startAsync cleared the listeners.
                        Reflect.tryCall1(Reflect.call(event, "getAsyncContext"), "addListener", proxy);
                        break;
                    default:
                        break;
                }
            } catch (Throwable e) {
                // never into the container
            } finally {
                t.busy = false;
            }
            return null;
        }

        private synchronized void finish(String error) {
            if (done) {
                return;
            }
            done = true;
            String detail = null;
            Object status = Reflect.call(response, "getStatus");
            if (status instanceof Integer) {
                int s = (Integer) status;
                detail = "status " + s;
                if (s >= 500 && error == null) {
                    error = "HTTP " + s;
                }
            }
            probe.recordFor(label, startEpochNanos, System.nanoTime() - start, Reflect.cut(error, 500), detail);
        }
    }

    /** {@code HttpServerHandle.onStateChange(connection, REQUEST_RECEIVED)} applies the handler. */
    private void reactorReceived(Call call, Object[] args) {
        Object ops = args[0];
        if (args.length < 2 || !Reflect.isA(ops, REACTOR_OPS) || !"REQUEST_RECEIVED".equals(Reflect.str(args[1], "name"))) {
            return;
        }
        received(call, ops, Reflect.str(ops, "method"), Reflect.str(ops, "uri"));
    }

    /** An asynchronous request was received; {@code key} is the object its completion is reported on. */
    private void received(Call call, Object key, String method, String uri) {
        if (key == null || method == null || uri == null) {
            return;
        }
        String label = method + " " + normalize(uri);
        pending.expunge();
        pending.put(key, new Pending(label, Probes.epochNanos(), System.nanoTime()));
        label(call, label);
    }

    /** Records a completed asynchronous request. */
    private void done(Pending p, Object status, Throwable failure) {
        long duration = System.nanoTime() - p.start;
        String detail = null;
        String error = null;
        if (status instanceof Integer) {
            int s = (Integer) status;
            detail = "status " + s;
            if (s >= 500) {
                error = "HTTP " + s;
            }
        }
        if (failure != null) {
            error = Reflect.cut(failure.getClass().getName() + (failure.getMessage() == null ? "" : ": " + failure.getMessage()), 500);
        }
        // Completions run on event loops or other threads: the event belongs to the request.
        recordFor(p.label, p.startEpochNanos, duration, error, detail);
    }

    /** The response of a Reactor Netty request completed or failed. */
    private void reactorDone(Object ops, Object[] args) {
        Pending p = pending.remove(ops);
        if (p == null) {
            return;
        }
        Object e = args != null && args.length > 0 ? args[0] : null;
        done(p, Reflect.call(Reflect.call(ops, "status"), "code"), e instanceof Throwable ? (Throwable) e : null);
    }

    /** {@code Http1xServerConnection.handleMessage(request)} creates the request and runs its handler. */
    private void vertxReceived(Call call, Object connection, Object msg) {
        if (!Reflect.isA(msg, "io.netty.handler.codec.http.HttpRequest")) {
            return;
        }
        String method = Reflect.str(msg, "method");
        String uri = Reflect.str(msg, "uri");
        if (method == null || uri == null) {
            return;
        }
        String label = method + " " + normalize(uri);
        vertx.expunge();
        ArrayDeque<Pending> q = vertx.computeIfAbsent(connection, c -> new ArrayDeque<>());
        synchronized (q) {
            q.addLast(new Pending(label, Probes.epochNanos(), System.nanoTime()));
        }
        label(call, label);
    }

    /** {@code Http1xServerConnection.responseComplete()}: the oldest request's response was written. */
    private void vertxDone(Object connection) {
        ArrayDeque<Pending> q = vertx.get(connection);
        Pending p;
        if (q == null) {
            return;
        }
        synchronized (q) {
            p = q.pollFirst();
        }
        if (p == null) {
            return;
        }
        Object request = Reflect.field(connection, "responseInProgress");
        done(p, Reflect.call(Reflect.call(request, "response"), "getStatusCode"), null);
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
