package matrix;

import dev.openprofiler.api.OpenProfiler;
import io.grpc.CallOptions;
import io.grpc.ManagedChannel;
import io.grpc.ManagedChannelBuilder;
import io.grpc.MethodDescriptor;
import io.grpc.Server;
import io.grpc.ServerBuilder;
import io.grpc.ServerServiceDefinition;
import io.grpc.Status;
import io.grpc.stub.ClientCalls;
import io.grpc.stub.ServerCalls;
import io.grpc.stub.StreamObserver;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;

/**
 * grpc-java over Netty without generated stubs: a unary service with string messages, called
 * blocking, through a future and asynchronously, plus a method that fails.
 */
public class Main {
    static final MethodDescriptor.Marshaller<String> STRING = new MethodDescriptor.Marshaller<String>() {
        @Override
        public InputStream stream(String value) {
            return new ByteArrayInputStream(value.getBytes(StandardCharsets.UTF_8));
        }

        @Override
        public String parse(InputStream stream) {
            try {
                ByteArrayOutputStream out = new ByteArrayOutputStream();
                byte[] buf = new byte[256];
                for (int n; (n = stream.read(buf)) > 0; ) {
                    out.write(buf, 0, n);
                }
                return new String(out.toByteArray(), StandardCharsets.UTF_8);
            } catch (IOException e) {
                throw new RuntimeException(e);
            }
        }
    };
    static final MethodDescriptor<String, String> GET = method("matrix.Owners/Get");
    static final MethodDescriptor<String, String> DELETE = method("matrix.Owners/Delete");
    static final ExecutorService POOL = Executors.newFixedThreadPool(2);

    static MethodDescriptor<String, String> method(String name) {
        return MethodDescriptor.<String, String>newBuilder()
            .setType(MethodDescriptor.MethodType.UNARY)
            .setFullMethodName(name)
            .setRequestMarshaller(STRING)
            .setResponseMarshaller(STRING)
            .build();
    }

    static void get(String req, StreamObserver<String> out) {
        try {
            String owner = POOL.submit(() -> {
                OpenProfiler.probeEvent("work", "load owner", 1000);
                return "owner " + req;
            }).get();
            out.onNext(owner);
            out.onCompleted();
        } catch (Exception e) {
            out.onError(e);
        }
    }

    public static void main(String[] args) throws Exception {
        ServerServiceDefinition svc = ServerServiceDefinition.builder("matrix.Owners")
            .addMethod(GET, ServerCalls.asyncUnaryCall(Main::get))
            .addMethod(DELETE, ServerCalls.asyncUnaryCall((String req, StreamObserver<String> out) ->
                out.onError(Status.PERMISSION_DENIED.withDescription("read only").asRuntimeException())))
            .build();
        Server server = ServerBuilder.forPort(0).addService(svc).build().start();
        ManagedChannel channel = ManagedChannelBuilder.forAddress("127.0.0.1", server.getPort()).usePlaintext().build();
        for (int i = 0; i < 20; i++) {
            ClientCalls.blockingUnaryCall(channel, GET, CallOptions.DEFAULT, String.valueOf(i));
        }
        for (int i = 0; i < 10; i++) {
            ClientCalls.futureUnaryCall(channel.newCall(GET, CallOptions.DEFAULT), "f" + i).get();
        }
        for (int i = 0; i < 3; i++) {
            try {
                ClientCalls.blockingUnaryCall(channel, DELETE, CallOptions.DEFAULT, "x");
            } catch (RuntimeException expected) {
                // PERMISSION_DENIED
            }
        }
        channel.shutdown().awaitTermination(5, TimeUnit.SECONDS);
        server.shutdown().awaitTermination(5, TimeUnit.SECONDS);
        POOL.shutdown();
    }
}
