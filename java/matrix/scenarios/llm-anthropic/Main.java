package matrix;

import com.anthropic.client.AnthropicClient;
import com.anthropic.client.okhttp.AnthropicOkHttpClient;
import com.anthropic.models.messages.MessageCreateParams;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;

/** The Anthropic Java SDK against a local fake: messages (blocking, async). */
public class Main {
    public static void main(String[] args) throws Exception {
        AnthropicClient client = AnthropicOkHttpClient.builder().apiKey("test").baseUrl(FakeLlm.start()).build();
        MessageCreateParams params = MessageCreateParams.builder()
            .model("claude-sonnet-4-5").maxTokens(64).addUserMessage("Say ok").build();
        for (int i = 0; i < 10; i++) {
            client.messages().create(params);
        }
        Requests.serve(5, id -> client.messages().create(params).id());
        List<CompletableFuture<?>> pending = new ArrayList<>();
        for (int i = 0; i < 5; i++) {
            pending.add(client.async().messages().create(params));
        }
        for (CompletableFuture<?> f : pending) {
            f.get();
        }
        client.close();
        FakeLlm.stop();
    }
}
