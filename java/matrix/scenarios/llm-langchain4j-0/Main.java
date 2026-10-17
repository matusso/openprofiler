package matrix;

import dev.langchain4j.model.openai.OpenAiChatModel;

/** LangChain4j 0.x {@code ChatLanguageModel} (OpenAI) against a local fake. */
public class Main {
    public static void main(String[] args) throws Exception {
        OpenAiChatModel model = OpenAiChatModel.builder().baseUrl(FakeLlm.start() + "/v1").apiKey("test")
            .modelName("gpt-4o-mini").build();
        for (int i = 0; i < 10; i++) {
            model.generate("Say ok");
        }
        Requests.serve(5, id -> model.generate("Say ok " + id));
        FakeLlm.stop();
    }
}
