package matrix;

import io.undertow.Undertow;
import io.undertow.servlet.Servlets;
import io.undertow.servlet.api.DeploymentInfo;
import io.undertow.servlet.api.DeploymentManager;
import jakarta.servlet.DispatcherType;
import java.net.InetSocketAddress;

/** Undertow with a servlet deployment (requests run on its worker threads). */
public class Main {
    public static void main(String[] args) throws Exception {
        DeploymentInfo di = Servlets.deployment()
            .setClassLoader(Main.class.getClassLoader())
            .setContextPath("/")
            .setDeploymentName("matrix")
            .addServlet(Servlets.servlet("owners", OwnerServlet.class).addMapping("/*").setAsyncSupported(true))
            .addFilter(Servlets.filter("timing", TimingFilter.class).setAsyncSupported(true))
            .addFilterUrlMapping("timing", "/*", DispatcherType.REQUEST);
        DeploymentManager manager = Servlets.defaultContainer().addDeployment(di);
        manager.deploy();
        Undertow server = Undertow.builder().addHttpListener(0, "127.0.0.1").setHandler(manager.start()).build();
        server.start();
        InetSocketAddress addr = (InetSocketAddress) server.getListenerInfo().get(0).getAddress();
        Load.run("http://127.0.0.1:" + addr.getPort());
        server.stop();
    }
}
