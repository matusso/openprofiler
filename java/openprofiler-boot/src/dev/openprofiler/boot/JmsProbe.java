package dev.openprofiler.boot;

/**
 * JMS (javax and Jakarta): messages sent ({@code MessageProducer}, {@code JMSProducer}), received
 * ({@code MessageConsumer.receive}, {@code JMSConsumer.receive*}; receives that time out are idle
 * waiting and not recorded) and processed by a {@code MessageListener}, which sets the request
 * label ({@code JMS orders}). Payloads name the queue or topic.
 */
final class JmsProbe extends Probe {
    private static final int SEND = 0, RECEIVE = 1, LISTENER = 2;
    private static final String[] EE = {"javax.jms.", "jakarta.jms."};

    JmsProbe() {
        super("jms", "JMS", "Destination", null);
    }

    @Override
    protected Interception[] interceptions() {
        Interception[] its = new Interception[EE.length * 6];
        int i = 0;
        for (String p : EE) {
            its[i++] = Interception.subtypes(p + "MessageProducer", "send", null).withArgs();
            its[i++] = Interception.subtypes(p + "JMSProducer", "send", null).withArgs();
            its[i++] = Interception.subtypes(p + "MessageConsumer", "receive", null);
            its[i++] = Interception.subtypes(p + "JMSConsumer", "receive", null);
            its[i++] = Interception.subtypes(p + "JMSConsumer", "receiveBody", null);
            its[i++] = Interception.subtypes(p + "MessageListener", "onMessage", null).withArgs();
        }
        return its;
    }

    /** The interception's kind: SEND, RECEIVE or LISTENER. */
    private static int kind(int interception) {
        switch (interception % 6) {
            case 0:
            case 1:
                return SEND;
            case 5:
                return LISTENER;
            default:
                return RECEIVE;
        }
    }

    /** A queue's or topic's name. */
    static String destination(Object d) {
        if (d == null) {
            return null;
        }
        String name = Reflect.str(d, "getQueueName");
        if (name == null) {
            name = Reflect.str(d, "getTopicName");
        }
        return name != null ? name : Reflect.cut(String.valueOf(d), 200);
    }

    private static boolean isDestination(Object o) {
        return Reflect.isA(o, "javax.jms.Destination") || Reflect.isA(o, "jakarta.jms.Destination");
    }

    /** Whether the closest enclosing call of this probe is a listener's (a request, not an operation). */
    private boolean inListener() {
        ProbeThread t = ProbeThread.current();
        if (t == null) {
            return false;
        }
        for (int i = t.depth - 2; i >= 0; i--) {
            Call c = t.calls[i];
            if (c.probe == this) {
                return kind(c.interception) == LISTENER;
            }
        }
        return false;
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        if (call.nested) {
            // What a listener does while processing is recorded, like work a server request does.
            if (kind(call.interception) == LISTENER || !inListener()) {
                return;
            }
            call.nested = false;
        }
        Object first = args != null && args.length > 0 ? args[0] : null;
        switch (kind(call.interception)) {
            case SEND: {
                Object d = isDestination(first) ? first : Reflect.call(self, "getDestination");
                if (d == null && first != null) {
                    d = Reflect.call(first, "getJMSDestination");
                }
                call.payload = "send " + (d == null ? "[unknown]" : destination(d));
                break;
            }
            case LISTENER: {
                String d = destination(Reflect.call(first, "getJMSDestination"));
                call.payload = "process " + (d == null ? "[unknown]" : d);
                label(call, "JMS " + (d == null ? "[unknown]" : d));
                break;
            }
            default:
                // Receives are named by the message they return.
                call.payload = "receive";
                break;
        }
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (call.payload == null || kind(call.interception) != RECEIVE) {
            return;
        }
        if (result == null && error == null) {
            // Timed out: idle waiting, not work.
            call.ignore();
            return;
        }
        String d = destination(Reflect.call(result, "getJMSDestination"));
        if (d != null) {
            call.payload = "receive " + d;
        }
    }
}
