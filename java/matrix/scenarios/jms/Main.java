package matrix;

import jakarta.jms.Connection;
import jakarta.jms.ConnectionFactory;
import jakarta.jms.Message;
import jakarta.jms.MessageConsumer;
import jakarta.jms.MessageListener;
import jakarta.jms.MessageProducer;
import jakarta.jms.Queue;
import jakarta.jms.Session;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * JMS against a broker: sends from a request handler and from main, synchronous receives (also one
 * that times out, which is not recorded) and a message listener that sends while processing.
 */
public class Main {
    public static void main(String[] args) throws Exception {
        ConnectionFactory factory = (ConnectionFactory) Class.forName("${jms.factory}")
            .getConstructor(String.class).newInstance("tcp://127.0.0.1:56161");
        String suffix = Long.toHexString(System.nanoTime());
        Connection connection = factory.createConnection();
        try {
            connection.start();
            Session session = connection.createSession(false, Session.AUTO_ACKNOWLEDGE);
            Queue orders = session.createQueue("orders-" + suffix);
            Queue events = session.createQueue("events-" + suffix);
            Queue audit = session.createQueue("audit-" + suffix);
            MessageProducer producer = session.createProducer(orders);
            Requests.serve(20, id -> {
                try {
                    producer.send(session.createTextMessage("order " + id));
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
                return "sent";
            });
            for (int i = 0; i < 30; i++) {
                producer.send(session.createTextMessage("order " + i));
            }
            MessageConsumer consumer = session.createConsumer(orders);
            int received = 0;
            while (received < 50 && consumer.receive(5000) != null) {
                received++;
            }
            consumer.receive(100);
            System.out.println("received " + received);
            if (received != 50) {
                throw new IllegalStateException("received " + received);
            }

            // A listener on its own session; it forwards to the audit queue while processing.
            Session listenerSession = connection.createSession(false, Session.AUTO_ACKNOWLEDGE);
            MessageProducer forward = listenerSession.createProducer(audit);
            CountDownLatch processed = new CountDownLatch(10);
            // Not a lambda: the probe instruments MessageListener implementations.
            listenerSession.createConsumer(events).setMessageListener(new MessageListener() {
                @Override
                public void onMessage(Message m) {
                    try {
                        forward.send(listenerSession.createTextMessage("seen"));
                    } catch (Exception e) {
                        throw new RuntimeException(e);
                    }
                    processed.countDown();
                }
            });
            MessageProducer eventProducer = session.createProducer(events);
            for (int i = 0; i < 10; i++) {
                eventProducer.send(session.createTextMessage("event " + i));
            }
            if (!processed.await(30, TimeUnit.SECONDS)) {
                throw new IllegalStateException("listener processed " + (10 - processed.getCount()));
            }
        } finally {
            connection.close();
        }
    }
}
