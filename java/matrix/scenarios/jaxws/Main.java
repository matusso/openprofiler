package matrix;

import jakarta.jws.WebMethod;
import jakarta.jws.WebService;
import jakarta.xml.ws.Dispatch;
import jakarta.xml.ws.Endpoint;
import jakarta.xml.ws.Service;
import java.io.StringReader;
import java.net.ServerSocket;
import java.net.URL;
import javax.xml.namespace.QName;
import javax.xml.transform.Source;
import javax.xml.transform.stream.StreamSource;

/**
 * A SOAP service published with {@code Endpoint.publish} and called in the same JVM through a
 * service proxy (also from a request handler) and a {@code Dispatch} client.
 */
public class Main {
    static final String NS = "http://matrix/";

    @WebService(name = "OrderService", targetNamespace = NS)
    public interface OrderService {
        @WebMethod
        String getOrder(int id);
    }

    @WebService(endpointInterface = "matrix.Main$OrderService", serviceName = "OrderService", portName = "OrderPort",
                targetNamespace = NS)
    public static class OrderServiceImpl implements OrderService {
        @Override
        public String getOrder(int id) {
            return price(id);
        }
    }

    /** Instrumented work done while serving, under the operation's request label. */
    static String price(int id) {
        long x = 0;
        for (int i = 0; i < 10_000; i++) {
            x += (i * 31L + id) % 7;
        }
        return "order " + id + " " + x;
    }

    public static void main(String[] args) throws Exception {
        int port;
        try (ServerSocket s = new ServerSocket(0)) {
            port = s.getLocalPort();
        }
        String address = "http://127.0.0.1:" + port + "/ws/orders";
        Endpoint endpoint = Endpoint.publish(address, new OrderServiceImpl());
        try {
            Service service = Service.create(new URL(address + "?wsdl"), new QName(NS, "OrderService"));
            OrderService orders = service.getPort(new QName(NS, "OrderPort"), OrderService.class);
            for (int i = 0; i < 20; i++) {
                orders.getOrder(i);
            }
            Requests.serve(10, id -> orders.getOrder(Integer.parseInt(id)));
            Dispatch<Source> dispatch = service.createDispatch(new QName(NS, "OrderPort"), Source.class,
                                                               Service.Mode.PAYLOAD);
            for (int i = 0; i < 5; i++) {
                dispatch.invoke(new StreamSource(new StringReader(
                    "<ns:getOrder xmlns:ns=\"" + NS + "\"><arg0>" + i + "</arg0></ns:getOrder>")));
            }
        } finally {
            endpoint.stop();
            // CXF's Jetty engine keeps its threads until the bus shuts down; threads that end
            // flush their call counts.
            try {
                Object bus = Class.forName("org.apache.cxf.BusFactory").getMethod("getDefaultBus").invoke(null);
                bus.getClass().getMethod("shutdown", boolean.class).invoke(bus, true);
            } catch (ClassNotFoundException metro) {
                // Metro runs on the JDK HTTP server, stopped with the endpoint.
            }
        }
    }
}
