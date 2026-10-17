package dev.openprofiler.boot;

/** Kafka clients: producer sends (by topic), consumer polls and commits. */
final class KafkaProbe extends Probe {
    private static final int SEND = 0, POLL = 1, COMMIT_SYNC = 2;

    KafkaProbe() {
        super("kafka", "Kafka", "Operation", null);
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.method("org.apache.kafka.clients.producer.KafkaProducer", "send", null).withArgs(),
            Interception.method("org.apache.kafka.clients.consumer.KafkaConsumer", "poll", null),
            Interception.method("org.apache.kafka.clients.consumer.KafkaConsumer", "commitSync", null),
        };
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        switch (call.interception) {
            case SEND: {
                Object record = args != null && args.length > 0 ? args[0] : null;
                call.payload = "send " + Reflect.str(record, "topic");
                break;
            }
            case POLL:
                call.payload = "poll " + Reflect.str(self, "subscription");
                break;
            case COMMIT_SYNC:
                call.payload = "commit " + Reflect.str(self, "subscription");
                break;
            default:
                break;
        }
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (call.interception == POLL) {
            Object n = Reflect.call(result, "count");
            if (n instanceof Integer) {
                call.detail = n + " records";
                if ((Integer) n == 0) {
                    // Empty polls are idle waiting, not work.
                    call.ignore();
                }
            }
        }
    }
}
