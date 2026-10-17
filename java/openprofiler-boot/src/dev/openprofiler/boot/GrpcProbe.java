package dev.openprofiler.boot;

/**
 * gRPC (grpc-java): blocking and future/async unary client calls, and unary server calls (the
 * service method runs when the request half-closes; its full method name becomes the request
 * label).
 */
final class GrpcProbe extends Probe {
    private static final int BLOCKING = 0, FUTURE = 1, ASYNC = 2, SERVER = 3;

    GrpcProbe() {
        super("grpc", "gRPC", "Method", null);
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.method("io.grpc.stub.ClientCalls", "blockingUnaryCall", null).withArgs(),
            Interception.method("io.grpc.stub.ClientCalls", "futureUnaryCall", null).withArgs(),
            Interception.method("io.grpc.stub.ClientCalls", "asyncUnaryCall", null).withArgs(),
            Interception.method("io.grpc.stub.ServerCalls$UnaryServerCallHandler$UnaryServerCallListener",
                                "onHalfClose", "()V"),
        };
    }

    private static String methodName(Object[] args) {
        if (args == null) {
            return null;
        }
        for (Object a : args) {
            if (Reflect.isA(a, "io.grpc.MethodDescriptor")) {
                return Reflect.str(a, "getFullMethodName");
            }
            if (Reflect.isA(a, "io.grpc.ClientCall")) {
                return callMethodName(a);
            }
        }
        return null;
    }

    /**
     * Full method name of a {@code ClientCall} (futureUnaryCall, asyncUnaryCall): no public API has
     * it, but the channel's call implementations keep the descriptor in a {@code method} field, and
     * interceptors' forwarding calls in a {@code delegate} field.
     */
    private static String callMethodName(Object call) {
        Object c = call;
        for (int i = 0; i < 6 && c != null; i++) {
            Object m = Reflect.field(c, "method");
            if (Reflect.isA(m, "io.grpc.MethodDescriptor")) {
                return Reflect.str(m, "getFullMethodName");
            }
            c = Reflect.field(c, "delegate");
        }
        return null;
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        if (call.nested) {
            return;
        }
        if (call.interception == SERVER) {
            Object serverCall = Reflect.field(self, "call");
            String name = Reflect.str(Reflect.call(serverCall, "getMethodDescriptor"), "getFullMethodName");
            if (name != null) {
                call.payload = "serve " + name;
                label(call, "gRPC " + name);
            }
            return;
        }
        String name = methodName(args);
        call.payload = (call.interception == BLOCKING ? "call " : "call (async) ") + (name == null ? "[unknown]" : name);
    }
}
