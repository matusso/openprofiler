package matrix;

import org.springframework.ai.chat.prompt.Prompt;
import org.springframework.ai.openai.OpenAiChatModel;
import org.springframework.ai.openai.OpenAiChatOptions;
import org.springframework.ai.openai.api.OpenAiApi;

/** A Spring AI {@code ChatModel} (OpenAI) against a local fake, without a Spring context. */
public class Main {
    public static void main(String[] args) throws Exception {
        OpenAiApi api = OpenAiApi.builder().baseUrl(FakeLlm.start()).apiKey("test").build();
        OpenAiChatModel model = OpenAiChatModel.builder().openAiApi(api)
            .defaultOptions(OpenAiChatOptions.builder().model("gpt-4o-mini").build()).build();
        for (int i = 0; i < 10; i++) {
            model.call(new Prompt("Say ok"));
        }
        Requests.serve(5, id -> model.call("Say ok " + id));
        FakeLlm.stop();
    }
}
