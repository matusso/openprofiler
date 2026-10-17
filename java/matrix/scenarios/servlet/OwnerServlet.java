package matrix;

import dev.openprofiler.api.OpenProfiler;
import jakarta.servlet.AsyncContext;
import jakarta.servlet.http.HttpServlet;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletResponse;
import java.io.IOException;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;

/**
 * {@code /owners/<id>} hands work to an executor and a CompletableFuture; {@code /fail} answers 500;
 * {@code /async} completes asynchronously on another thread 20 ms later.
 */
public class OwnerServlet extends HttpServlet {
    private static final long serialVersionUID = 1L;
    static final ExecutorService POOL = Executors.newFixedThreadPool(2);

    @Override
    protected void doGet(HttpServletRequest req, HttpServletResponse resp) throws IOException {
        if (req.getRequestURI().startsWith("/async")) {
            AsyncContext ac = req.startAsync();
            POOL.submit(() -> {
                try {
                    Thread.sleep(20);
                    OpenProfiler.probeEvent("work", "async part", 1000);
                    ac.getResponse().getWriter().write("done");
                } catch (Exception e) {
                    throw new IllegalStateException(e);
                } finally {
                    ac.complete();
                }
            });
            return;
        }
        if (req.getRequestURI().startsWith("/fail")) {
            resp.setStatus(500);
            resp.getWriter().write("failed");
            return;
        }
        try {
            Future<?> f = POOL.submit(() -> OpenProfiler.probeEvent("work", "load owner", 1000));
            String price = CompletableFuture.supplyAsync(() -> {
                OpenProfiler.probeEvent("work", "price", 1000);
                return "12";
            }).get();
            f.get();
            Thread.sleep(2);
            resp.setContentType("text/plain");
            resp.getWriter().write(render(req.getRequestURI(), price));
        } catch (Exception e) {
            throw new IOException(e);
        }
    }

    static String render(String uri, String price) {
        return uri + " " + price;
    }
}
