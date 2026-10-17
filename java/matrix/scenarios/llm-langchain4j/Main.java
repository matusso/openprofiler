package matrix;

import dev.langchain4j.model.openai.OpenAiChatModel;

/** LangChain4j 1.x {@code ChatModel} (OpenAI, its own HTTP client) against a local fake. */
public class Main {
    public static void main(String[] args) throws Exception {
        OpenAiChatModel model = OpenAiChatModel.builder().baseUrl(FakeLlm.start() + "/v1").apiKey("test")
            .modelName("gpt-4o-mini").build();
        for (int i = 0; i < 10; i++) {
            model.chat("Say ok");
        }
        Requests.serve(5, id -> model.chat("Say ok " + id));
        FakeLlm.stop();
    }
}
