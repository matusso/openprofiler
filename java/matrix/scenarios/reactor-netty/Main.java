package matrix;

import dev.openprofiler.api.OpenProfiler;
import java.time.Duration;
import reactor.core.publisher.Mono;
import reactor.core.scheduler.Schedulers;
import reactor.netty.DisposableServer;
import reactor.netty.http.server.HttpServer;

/**
 * Reactor Netty routes whose responses complete later on other threads: a blocking lookup on
 * {@code boundedElastic}, then a delay on the {@code parallel} scheduler; and a route that fails.
 */
public class Main {
    static Mono<String> owner(String id) {
        return Mono.fromCallable(() -> {
                OpenProfiler.probeEvent("work", "load owner", 1000);
                return id;
            })
            .subscribeOn(Schedulers.boundedElastic())
            .delayElement(Duration.ofMillis(20))
            .map(Main::render);
    }

    static String render(String id) {
        OpenProfiler.probeEvent("work", "render", 1000);
        return "owner " + id;
    }

    public static void main(String[] args) throws Exception {
        DisposableServer server = HttpServer.create().host("127.0.0.1").port(0)
            .route(r -> r
                .get("/owners/{id}", (req, res) -> res.sendString(owner(req.param("id"))))
                .get("/fail", (req, res) -> Mono.error(new IllegalStateException("boom")))
                .get("/ping", (req, res) -> res.sendString(Mono.just("pong"))))
            .bindNow();
        String base = "http://127.0.0.1:" + server.port();
        Http.await(base + "/ping");
        for (int i = 0; i < 30; i++) {
            Http.get(base + "/owners/" + (1 + i % 5) + "?x=1");
        }
        for (int i = 0; i < 3; i++) {
            Http.get(base + "/fail");
        }
        server.disposeNow();
    }
}
