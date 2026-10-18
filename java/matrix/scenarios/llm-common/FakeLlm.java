package matrix;

import com.sun.net.httpserver.HttpServer;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;

/**
 * A local stand-in for the OpenAI and Anthropic HTTP APIs with canned responses (12 tokens in, 3
 * out), so client SDKs run without network access or keys.
 */
final class FakeLlm {
    static final String CHAT = "{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"created\":1700000000,"
        + "\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"ok\","
        + "\"refusal\":null},\"finish_reason\":\"stop\",\"logprobs\":null}],"
        + "\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15}}";
    static final String EMBEDDINGS = "{\"object\":\"list\",\"data\":[{\"object\":\"embedding\",\"index\":0,"
        + "\"embedding\":[0.1,0.2,0.3]}],\"model\":\"text-embedding-3-small\","
        + "\"usage\":{\"prompt_tokens\":4,\"total_tokens\":4}}";
    static final String MESSAGES = "{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\","
        + "\"model\":\"claude-sonnet-4-5\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],"
        + "\"stop_reason\":\"end_turn\",\"stop_sequence\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}";

    private static HttpServer server;

    private FakeLlm() {}

    /** Starts the server; returns its base URL ({@code http://127.0.0.1:<port>}). */
    static String start() throws Exception {
        server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/", ex -> {
            ex.getRequestBody().readAllBytes();
            String path = ex.getRequestURI().getPath();
            String body = path.endsWith("/chat/completions") ? CHAT
                : path.endsWith("/embeddings") ? EMBEDDINGS
                : path.endsWith("/messages") ? MESSAGES : null;
            byte[] bytes = (body == null ? "{}" : body).getBytes(StandardCharsets.UTF_8);
            ex.getResponseHeaders().add("Content-Type", "application/json");
            ex.sendResponseHeaders(body == null ? 404 : 200, bytes.length);
            ex.getResponseBody().write(bytes);
            ex.close();
        });
        server.setExecutor(java.util.concurrent.Executors.newFixedThreadPool(4, r -> {
            Thread t = new Thread(r, "fake-llm");
            t.setDaemon(true);
            return t;
        }));
        server.start();
        return "http://127.0.0.1:" + server.getAddress().getPort();
    }

    static void stop() {
        server.stop(0);
    }
}
