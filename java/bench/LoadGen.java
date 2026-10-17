import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.atomic.AtomicLong;

/**
 * A closed-loop HTTP load generator for the overhead benchmark: {@code threads} threads request
 * the URLs round-robin for {@code seconds} and print one line of results (requests per second,
 * latency percentiles in ms, errors).
 *
 * <pre>java LoadGen.java &lt;seconds&gt; &lt;threads&gt; &lt;url&gt;...</pre>
 */
public class LoadGen {
    public static void main(String[] args) throws Exception {
        int seconds = Integer.parseInt(args[0]);
        int threads = Integer.parseInt(args[1]);
        List<URI> urls = new ArrayList<>();
        for (int i = 2; i < args.length; i++) {
            urls.add(URI.create(args[i]));
        }
        HttpClient client = HttpClient.newBuilder().version(HttpClient.Version.HTTP_1_1)
            .connectTimeout(Duration.ofSeconds(5)).build();
        long end = System.nanoTime() + seconds * 1_000_000_000L;
        AtomicLong errors = new AtomicLong();
        long[][] latencies = new long[threads][];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            int id = t;
            ts[t] = new Thread(() -> {
                long[] lat = new long[1 << 16];
                int n = 0;
                int next = id;
                while (System.nanoTime() < end) {
                    HttpRequest req = HttpRequest.newBuilder(urls.get(next++ % urls.size()))
                        .timeout(Duration.ofSeconds(30)).build();
                    long start = System.nanoTime();
                    try {
                        int status = client.send(req, HttpResponse.BodyHandlers.discarding()).statusCode();
                        if (status >= 400) {
                            errors.incrementAndGet();
                        }
                    } catch (Exception e) {
                        errors.incrementAndGet();
                    }
                    if (n == lat.length) {
                        lat = Arrays.copyOf(lat, n * 2);
                    }
                    lat[n++] = System.nanoTime() - start;
                }
                latencies[id] = Arrays.copyOf(lat, n);
            });
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long[] all = Arrays.stream(latencies).flatMapToLong(Arrays::stream).sorted().toArray();
        System.out.printf("requests=%d rps=%.1f p50=%.3f p90=%.3f p99=%.3f errors=%d%n", all.length,
            all.length / (double) seconds, pct(all, 0.50), pct(all, 0.90), pct(all, 0.99), errors.get());
    }

    static double pct(long[] sorted, double p) {
        return sorted.length == 0 ? 0 : sorted[(int) Math.min(sorted.length - 1, sorted.length * p)] / 1e6;
    }
}
