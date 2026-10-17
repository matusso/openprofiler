package matrix;

import dev.openprofiler.api.OpenProfiler;
import java.time.Duration;
import reactor.core.publisher.Flux;
import reactor.core.publisher.Mono;
import reactor.core.scheduler.Schedulers;

/** Reactor pipelines started by a request, hopping to every kind of scheduler. */
public class Main {
    static String work(String what) {
        OpenProfiler.probeEvent("work", what, 1000);
        return what;
    }

    static String handle(String id) {
        Mono.fromCallable(() -> work("bounded elastic")).subscribeOn(Schedulers.boundedElastic()).block();
        Mono.just(id).publishOn(Schedulers.single()).map(x -> work("single")).block();
        Mono.delay(Duration.ofMillis(5)).map(x -> work("after delay")).block();
        Flux.range(1, 4).parallel(2).runOn(Schedulers.parallel()).map(i -> work("parallel")).sequential().blockLast();
        return id;
    }

    public static void main(String[] args) throws Exception {
        Requests.serve(20, Main::handle);
        Schedulers.shutdownNow();
    }
}
