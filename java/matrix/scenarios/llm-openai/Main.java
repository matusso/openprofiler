package matrix;

import com.openai.client.OpenAIClient;
import com.openai.client.okhttp.OpenAIOkHttpClient;
import com.openai.models.ChatModel;
import com.openai.models.chat.completions.ChatCompletionCreateParams;
import com.openai.models.embeddings.EmbeddingCreateParams;
import com.openai.models.embeddings.EmbeddingModel;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;

/** The OpenAI Java SDK against a local fake: chat completions (blocking, async), embeddings. */
public class Main {
    public static void main(String[] args) throws Exception {
        OpenAIClient client = OpenAIOkHttpClient.builder().apiKey("test").baseUrl(FakeLlm.start() + "/v1").build();
        ChatCompletionCreateParams chat = ChatCompletionCreateParams.builder()
            .model(ChatModel.GPT_4O_MINI).addUserMessage("Say ok").build();
        for (int i = 0; i < 10; i++) {
            client.chat().completions().create(chat);
        }
        Requests.serve(5, id -> client.chat().completions().create(chat).id());
        List<CompletableFuture<?>> pending = new ArrayList<>();
        for (int i = 0; i < 5; i++) {
            pending.add(client.async().chat().completions().create(chat));
        }
        for (CompletableFuture<?> f : pending) {
            f.get();
        }
        client.embeddings().create(EmbeddingCreateParams.builder()
            .model(EmbeddingModel.TEXT_EMBEDDING_3_SMALL).input("hello").build());
        client.close();
        FakeLlm.stop();
    }
}
