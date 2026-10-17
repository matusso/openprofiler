package dev.openprofiler.boot;

import java.lang.reflect.Method;
import java.util.Map;

/**
 * SOAP web services (JAX-WS, javax and Jakarta): client calls through service proxies (Metro's
 * {@code SEIStub}, CXF's {@code JaxWsClientProxy}) and {@code Dispatch} clients, and operations
 * served (Metro's endpoint invokers, CXF's {@code AbstractInvoker}), which set the request label
 * ({@code SOAP OrderService.getOrder}). Client calls carry the endpoint address as detail.
 */
final class WebServiceProbe extends Probe {
    private static final int METRO_CLIENT = 0, CXF_CLIENT = 1, METRO_SERVER = 2, CXF_SERVER = 3, DISPATCH = 4;
    private static final String[] ADDRESS = {
        "jakarta.xml.ws.service.endpoint.address", "javax.xml.ws.service.endpoint.address",
    };

    WebServiceProbe() {
        super("webservices", "Web services", "Operation", null);
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.method("com.sun.xml.ws.client.sei.SEIStub", "invoke", null).withArgs(),
            Interception.method("org.apache.cxf.jaxws.JaxWsClientProxy", "invoke", null).withArgs(),
            Interception.subtypes("com.sun.xml.ws.server.sei.Invoker", "invoke", null).withArgs(),
            Interception.subtypes("org.apache.cxf.service.invoker.AbstractInvoker", "performInvocation", null).withArgs(),
            Interception.subtypes("javax.xml.ws.Dispatch", "invoke", null),
            Interception.subtypes("jakarta.xml.ws.Dispatch", "invoke", null),
            Interception.subtypes("javax.xml.ws.Dispatch", "invokeOneWay", null),
            Interception.subtypes("jakarta.xml.ws.Dispatch", "invokeOneWay", null),
        };
    }

    private static Method method(Object[] args) {
        if (args != null) {
            for (Object a : args) {
                if (a instanceof Method) {
                    return (Method) a;
                }
            }
        }
        return null;
    }

    /** The endpoint address of a {@code BindingProvider} (a proxy or a dispatch). */
    private static String address(Object provider) {
        Object ctx = Reflect.call(provider, "getRequestContext");
        if (ctx instanceof Map) {
            for (String key : ADDRESS) {
                Object a = ((Map<?, ?>) ctx).get(key);
                if (a != null) {
                    return Reflect.cut(a.toString(), 500);
                }
            }
        }
        return null;
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        if (call.nested) {
            return;
        }
        int kind = Math.min(call.interception, DISPATCH);
        switch (kind) {
            case METRO_CLIENT:
            case CXF_CLIENT: {
                Method m = method(args);
                if (m == null || m.getDeclaringClass() == Object.class) {
                    return;
                }
                call.payload = "call " + m.getDeclaringClass().getSimpleName() + "." + m.getName();
                call.detail = address(args != null && args.length > 0 ? args[0] : null);
                break;
            }
            case METRO_SERVER:
            case CXF_SERVER: {
                Method m = method(args);
                if (m == null) {
                    return;
                }
                // The implementation's interface (its endpoint), else the implementation class.
                String type = m.getDeclaringClass().getSimpleName();
                call.payload = "serve " + type + "." + m.getName();
                label(call, "SOAP " + type + "." + m.getName());
                break;
            }
            default: {
                String address = address(self);
                call.payload = "dispatch " + (address == null ? "[unknown]" : HttpClientProbe.normalize(address));
                break;
            }
        }
    }
}
