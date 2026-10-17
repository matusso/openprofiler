package matrix;

import dev.openprofiler.api.OpenProfiler;
import java.time.Duration;
import org.springframework.http.server.reactive.HttpHandler;
import org.springframework.http.server.reactive.ReactorHttpHandlerAdapter;
import org.springframework.web.reactive.function.server.RouterFunction;
import org.springframework.web.reactive.function.server.RouterFunctions;
import org.springframework.web.reactive.function.server.ServerResponse;
import reactor.core.publisher.Mono;
import reactor.core.scheduler.Schedulers;
import reactor.netty.DisposableServer;
import reactor.netty.http.server.HttpServer;

/** Spring WebFlux functional routes on Reactor Netty (what Spring Boot runs by default). */
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
        RouterFunction<ServerResponse> routes = RouterFunctions.route()
            .GET("/owners/{id}", req -> owner(req.pathVariable("id")).flatMap(s -> ServerResponse.ok().bodyValue(s)))
            .GET("/fail", req -> Mono.error(new IllegalStateException("boom")))
            .GET("/ping", req -> ServerResponse.ok().bodyValue("pong"))
            .build();
        HttpHandler handler = RouterFunctions.toHttpHandler(routes);
        DisposableServer server = HttpServer.create().host("127.0.0.1").port(0)
            .handle(new ReactorHttpHandlerAdapter(handler)).bindNow();
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
