package dev.openprofiler.boot;

import java.util.concurrent.ConcurrentHashMap;

/**
 * Custom probes: methods annotated with {@code @dev.openprofiler.api.Probe} (instrumented by the
 * agent, which names the probe {@code custom.<name>}) and calls recorded through
 * {@code OpenProfiler.probeEvent}. The payload template may refer to arguments ({@code {0}},
 * {@code {1}}, ...) and to the receiver ({@code {this}}).
 */
final class CustomProbe extends Probe {
    /** Agent probe index → handler for annotated sites (the agent registers those probes itself). */
    private static final ConcurrentHashMap<Integer, CustomProbe> BY_AGENT_INDEX = new ConcurrentHashMap<>();
    private static final ConcurrentHashMap<Integer, String> TEMPLATES = new ConcurrentHashMap<>();

    CustomProbe(String name) {
        super("custom." + name, name, "Payload", null);
    }

    private CustomProbe(int agentIndex) {
        super("custom", "Custom", "Payload", null);
        index = agentIndex;
    }

    static CustomProbe forSite(int agentIndex, int site) {
        CustomProbe p = BY_AGENT_INDEX.get(agentIndex);
        if (p == null) {
            p = new CustomProbe(agentIndex);
            BY_AGENT_INDEX.put(agentIndex, p);
        }
        return p;
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[0];
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        String t = TEMPLATES.get(call.site);
        if (t == null) {
            t = Hooks.siteTemplate0(call.site);
            TEMPLATES.put(call.site, t == null ? "" : t);
        }
        call.payload = format(t, self, args);
    }

    static String format(String template, Object self, Object[] args) {
        if (template == null || template.indexOf('{') < 0) {
            return template;
        }
        StringBuilder sb = new StringBuilder();
        int i = 0;
        while (i < template.length()) {
            char ch = template.charAt(i);
            int close = ch == '{' ? template.indexOf('}', i) : -1;
            if (close < 0) {
                sb.append(ch);
                i++;
                continue;
            }
            String key = template.substring(i + 1, close);
            Object v;
            if (key.equals("this")) {
                v = self;
            } else {
                try {
                    int n = Integer.parseInt(key);
                    v = args != null && n >= 0 && n < args.length ? args[n] : "?";
                } catch (NumberFormatException e) {
                    v = "{" + key + "}";
                }
            }
            sb.append(Reflect.cut(String.valueOf(v), 200));
            i = close + 1;
        }
        return sb.toString();
    }
}
