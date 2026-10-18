package dev.openprofiler.boot;

import java.util.Optional;

/**
 * AI/LLM clients: the OpenAI Java SDK (chat completions, responses, embeddings; blocking and
 * async), the Anthropic Java SDK (messages; blocking and async), LangChain4j chat models (0.x
 * {@code ChatLanguageModel}, 1.x {@code ChatModel}) and Spring AI chat models. The payload is the
 * operation and the model ({@code chat.completions gpt-4o-mini}); the detail has the tokens used.
 * Async calls are timed until their future completes. Streaming calls are not recorded.
 */
final class LlmProbe extends Probe {
    private static final int OPENAI_CHAT = 0, OPENAI_RESPONSES = 1, OPENAI_EMBEDDINGS = 2, ANTHROPIC_MESSAGES = 3,
        LC4J_CHAT = 4, LC4J_LEGACY_CHAT = 5, LC4J_LEGACY_GENERATE = 6, SPRING_AI_CHAT = 7;
    /** Interceptions from here on are the async variants of the first ones. */
    private static final int ASYNC = 8;
    private static final String LC4J_REQUEST =
        "(Ldev/langchain4j/model/chat/request/ChatRequest;)Ldev/langchain4j/model/chat/response/ChatResponse;";

    LlmProbe() {
        super("llm", "AI/LLM", "Model", null);
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.subtypes("com.openai.services.blocking.chat.ChatCompletionService", "create", null).withArgs(),
            Interception.subtypes("com.openai.services.blocking.ResponseService", "create", null).withArgs(),
            Interception.subtypes("com.openai.services.blocking.EmbeddingService", "create", null).withArgs(),
            Interception.subtypes("com.anthropic.services.blocking.MessageService", "create", null).withArgs(),
            // The overloads that every other one ends in, so that the recorded call has the usage.
            Interception.subtypes("dev.langchain4j.model.chat.ChatModel", "chat", LC4J_REQUEST).withArgs(),
            Interception.subtypes("dev.langchain4j.model.chat.ChatLanguageModel", "chat", LC4J_REQUEST).withArgs(),
            Interception.subtypes("dev.langchain4j.model.chat.ChatLanguageModel", "generate",
                                  "(Ljava/util/List;)Ldev/langchain4j/model/output/Response;").withArgs(),
            Interception.subtypes("org.springframework.ai.chat.model.ChatModel", "call",
                                  "(Lorg/springframework/ai/chat/prompt/Prompt;)Lorg/springframework/ai/chat/model/ChatResponse;")
                .withArgs(),
            Interception.subtypes("com.openai.services.async.chat.ChatCompletionServiceAsync", "create", null).withArgs(),
            Interception.subtypes("com.openai.services.async.ResponseServiceAsync", "create", null).withArgs(),
            Interception.subtypes("com.openai.services.async.EmbeddingServiceAsync", "create", null).withArgs(),
            Interception.subtypes("com.anthropic.services.async.MessageServiceAsync", "create", null).withArgs(),
        };
    }

    private static Object unwrap(Object o) {
        return o instanceof Optional ? ((Optional<?>) o).orElse(null) : o;
    }

    /** A model's name: a string, an SDK's model enum or a union of both. */
    static String model(Object m) {
        for (int i = 0; i < 3; i++) {
            m = unwrap(m);
            if (m == null || m instanceof String) {
                return (String) m;
            }
            Object s = Reflect.call(m, "asString");
            if (s instanceof String) {
                return (String) s;
            }
            // OpenAI's ResponsesModel: a string or one of two enums.
            Object next = unwrap(Reflect.call(m, "string"));
            if (next == null) {
                next = unwrap(Reflect.call(m, "chat"));
            }
            if (next == null) {
                next = unwrap(Reflect.call(m, "only"));
            }
            if (next == null) {
                return Reflect.cut(String.valueOf(m), 100);
            }
            m = next;
        }
        return null;
    }

    private static String operation(int interception) {
        switch (interception % ASYNC) {
            case OPENAI_CHAT:
                return "chat.completions";
            case OPENAI_RESPONSES:
                return "responses";
            case OPENAI_EMBEDDINGS:
                return "embeddings";
            case ANTHROPIC_MESSAGES:
                return "messages";
            case LC4J_LEGACY_GENERATE:
                return "generate";
            default:
                return "chat";
        }
    }

    /** The model a call asks for, if its request says. */
    private static String requestedModel(int interception, Object self, Object request) {
        switch (interception % ASYNC) {
            case LC4J_CHAT:
            case LC4J_LEGACY_CHAT: {
                String m = Reflect.str(Reflect.call(request, "parameters"), "modelName");
                if (m == null) {
                    m = Reflect.str(Reflect.call(self, "defaultRequestParameters"), "modelName");
                }
                return m != null ? m : fieldModel(self);
            }
            case LC4J_LEGACY_GENERATE:
                return fieldModel(self);
            case SPRING_AI_CHAT: {
                String m = Reflect.str(Reflect.call(request, "getOptions"), "getModel");
                return m != null ? m : Reflect.str(Reflect.call(self, "getDefaultOptions"), "getModel");
            }
            default:
                return model(Reflect.call(request, "model"));
        }
    }

    private static String fieldModel(Object self) {
        Object m = Reflect.field(self, "modelName");
        return m instanceof String ? (String) m : null;
    }

    /** The model a response names. */
    private static String responseModel(Object response) {
        String m = model(Reflect.call(response, "model"));
        if (m == null) {
            m = Reflect.str(response, "modelName");
        }
        if (m == null) {
            m = Reflect.str(Reflect.call(response, "getMetadata"), "getModel");
        }
        return m;
    }

    /** {@code tokens <in> in, <out> out} from a response's usage, if it has one. */
    static String tokens(Object response) {
        Object usage = unwrap(Reflect.call(response, "usage"));
        if (usage == null) {
            usage = Reflect.call(response, "tokenUsage");
        }
        if (usage == null) {
            usage = Reflect.call(Reflect.call(response, "getMetadata"), "getUsage");
        }
        if (usage == null) {
            return null;
        }
        Object in = first(usage, "promptTokens", "inputTokens", "inputTokenCount", "getPromptTokens");
        Object out = first(usage, "completionTokens", "outputTokens", "outputTokenCount", "getCompletionTokens");
        if (in == null && out == null) {
            return null;
        }
        return "tokens " + (in == null ? "?" : in) + " in" + (out == null ? "" : ", " + out + " out");
    }

    private static Object first(Object target, String... names) {
        for (String n : names) {
            Object v = unwrap(Reflect.call(target, n));
            if (v instanceof Number) {
                return v;
            }
        }
        return null;
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        if (call.nested) {
            return;
        }
        Object request = args != null && args.length > 0 ? args[0] : null;
        String model = requestedModel(call.interception, self, request);
        call.state = model;
        call.payload = operation(call.interception) + (model == null ? "" : " " + model);
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (call.payload == null || error != null || result == null) {
            return;
        }
        if (call.interception >= ASYNC) {
            recordOnCompletion(call, result, LlmProbe::tokens);
            return;
        }
        if (call.state == null) {
            String model = responseModel(result);
            if (model != null) {
                call.payload = operation(call.interception) + " " + model;
            }
        }
        call.detail = tokens(result);
    }
}
