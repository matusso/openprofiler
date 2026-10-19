package dev.openprofiler.boot;

import java.lang.reflect.Method;

/**
 * Java RMI: remote calls made through stubs (dynamic proxies and generated stubs both end in
 * {@code UnicastRef.invoke}) and calls served ({@code UnicastServerRef.dispatch}). A served call's
 * method is known once {@code dispatch} has read it ({@code logCall}); it becomes the payload and
 * the request label ({@code RMI OrderService.price}). Calls served by skeletons (the registry, DGC)
 * are not recorded.
 */
final class RmiProbe extends Probe {
    private static final int CALL = 0, DISPATCH = 1, LOG_CALL = 2;

    RmiProbe() {
        super("rmi", "RMI", "Method", null);
    }

    /** Dispatch calls get their payload from {@code logCall}. */
    @Override
    boolean alwaysTimed() {
        return true;
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.method("sun.rmi.server.UnicastRef", "invoke",
                                "(Ljava/rmi/Remote;Ljava/lang/reflect/Method;[Ljava/lang/Object;J)Ljava/lang/Object;")
                .withArgs(),
            Interception.method("sun.rmi.server.UnicastServerRef", "dispatch",
                                "(Ljava/rmi/Remote;Ljava/rmi/server/RemoteCall;)V"),
            Interception.method("sun.rmi.server.UnicastServerRef", "logCall", "(Ljava/rmi/Remote;Ljava/lang/Object;)V")
                .withArgs().entryOnly(),
        };
    }

    static String name(Object method) {
        if (method instanceof Method) {
            Method m = (Method) method;
            return m.getDeclaringClass().getSimpleName() + "." + m.getName();
        }
        return method == null ? "[unknown]" : Reflect.cut(String.valueOf(method), 200);
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        switch (call.interception) {
            case CALL:
                if (!call.nested && args != null && args.length > 1) {
                    call.payload = "call " + name(args[1]);
                }
                break;
            case LOG_CALL: {
                // Names the dispatch call this runs in.
                ProbeThread t = ProbeThread.current();
                if (t == null || args == null || args.length < 2 || !(args[1] instanceof Method)) {
                    break;
                }
                for (int i = t.depth - 1; i >= 0; i--) {
                    Call c = t.calls[i];
                    if (c.probe == this && c.interception == DISPATCH) {
                        if (c.payload == null && !c.nested) {
                            String name = name(args[1]);
                            c.payload = "serve " + name;
                            label(c, "RMI " + name);
                        }
                        break;
                    }
                }
                break;
            }
            default:
                break;
        }
    }
}
