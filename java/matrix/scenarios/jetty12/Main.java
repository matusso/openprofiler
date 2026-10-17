package matrix;

import jakarta.servlet.DispatcherType;
import java.util.EnumSet;
import org.eclipse.jetty.server.Server;
import org.eclipse.jetty.server.ServerConnector;
import org.eclipse.jetty.${jetty.ee}.servlet.FilterHolder;
import org.eclipse.jetty.${jetty.ee}.servlet.ServletContextHandler;
import org.eclipse.jetty.${jetty.ee}.servlet.ServletHolder;

/** Jetty 12 (ee8, ee9 and ee10 environments) with a servlet context. */
public class Main {
    public static void main(String[] args) throws Exception {
        Server server = new Server(0);
        ServletContextHandler ctx = new ServletContextHandler();
        ServletHolder servlet = new ServletHolder(new OwnerServlet());
        servlet.setAsyncSupported(true);
        ctx.addServlet(servlet, "/*");
        FilterHolder filter = new FilterHolder(new TimingFilter());
        filter.setAsyncSupported(true);
        ctx.addFilter(filter, "/*", EnumSet.of(DispatcherType.REQUEST));
        server.setHandler(ctx);
        server.start();
        Load.run("http://127.0.0.1:" + ((ServerConnector) server.getConnectors()[0]).getLocalPort());
        server.stop();
    }
}
