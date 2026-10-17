package matrix;

import java.nio.file.Files;
import org.apache.catalina.Context;
import org.apache.catalina.startup.Tomcat;
import org.apache.tomcat.util.descriptor.web.FilterDef;
import org.apache.tomcat.util.descriptor.web.FilterMap;

/** Embedded Tomcat. */
public class Main {
    public static void main(String[] args) throws Exception {
        Tomcat tomcat = new Tomcat();
        tomcat.setBaseDir(Files.createTempDirectory("tomcat").toString());
        tomcat.setPort(0);
        tomcat.getConnector();
        Context ctx = tomcat.addContext("", null);
        Tomcat.addServlet(ctx, "owners", new OwnerServlet()).setAsyncSupported(true);
        ctx.addServletMappingDecoded("/*", "owners");
        FilterDef fd = new FilterDef();
        fd.setFilterName("timing");
        fd.setFilter(new TimingFilter());
        fd.setAsyncSupported("true");
        ctx.addFilterDef(fd);
        FilterMap fm = new FilterMap();
        fm.setFilterName("timing");
        fm.addURLPattern("/*");
        ctx.addFilterMap(fm);
        tomcat.start();
        Load.run("http://127.0.0.1:" + tomcat.getConnector().getLocalPort());
        tomcat.stop();
        tomcat.destroy();
    }
}
