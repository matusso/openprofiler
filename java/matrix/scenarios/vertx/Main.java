package matrix;

import dev.openprofiler.api.OpenProfiler;
import io.vertx.core.Vertx;
import io.vertx.core.http.HttpServer;
import io.vertx.ext.web.Router;

/**
 * Vert.x Web: a route that runs blocking work on the worker pool, one that answers from a timer
 * 20 ms later, and one that fails; requested over HTTP/1.1 and over HTTP/2 (h2c upgrade).
 */
public class Main {
    static String render(String id) {
        OpenProfiler.probeEvent("work", "load owner", 1000);
        return "owner " + id;
    }

    public static void main(String[] args) throws Exception {
        Vertx vertx = Vertx.vertx();
        Router router = Router.router(vertx);
        router.get("/owners/:id").handler(ctx -> vertx.executeBlocking(() -> render(ctx.pathParam("id")))
            .onSuccess(body -> ctx.response().end(body)));
        router.get("/later").handler(ctx -> vertx.setTimer(20, t -> ctx.response().end("later")));
        router.get("/fail").handler(ctx -> ctx.fail(500));
        router.get("/ping").handler(ctx -> ctx.response().end("pong"));
        HttpServer server = vertx.createHttpServer().requestHandler(router).listen(0, "127.0.0.1")
            .toCompletionStage().toCompletableFuture().get();
        String base = "http://127.0.0.1:" + server.actualPort();
        Http.await(base + "/ping");
        for (int i = 0; i < 15; i++) {
            Http.get(base + "/owners/" + (1 + i % 5) + "?x=1");
            Http.get1(base + "/owners/" + (1 + i % 5) + "?x=1");
        }
        for (int i = 0; i < 5; i++) {
            Http.get(base + "/later");
            Http.get1(base + "/later");
        }
        for (int i = 0; i < 3; i++) {
            Http.get(base + "/fail");
        }
        Http.get1(base + "/fail");
        vertx.close().toCompletionStage().toCompletableFuture().get();
    }
}
