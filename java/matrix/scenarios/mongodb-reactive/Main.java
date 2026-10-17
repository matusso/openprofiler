package matrix;

import com.mongodb.reactivestreams.client.MongoClient;
import com.mongodb.reactivestreams.client.MongoClients;
import com.mongodb.reactivestreams.client.MongoCollection;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import org.bson.Document;
import org.reactivestreams.Publisher;
import org.reactivestreams.Subscriber;
import org.reactivestreams.Subscription;

/** The reactive streams MongoDB driver: commands complete on the driver's threads. */
public class Main {
    static <T> List<T> await(Publisher<T> p) throws Exception {
        CompletableFuture<List<T>> done = new CompletableFuture<>();
        List<T> items = new ArrayList<>();
        p.subscribe(new Subscriber<T>() {
            @Override
            public void onSubscribe(Subscription s) {
                s.request(Long.MAX_VALUE);
            }

            @Override
            public void onNext(T t) {
                items.add(t);
            }

            @Override
            public void onError(Throwable t) {
                done.completeExceptionally(t);
            }

            @Override
            public void onComplete() {
                done.complete(items);
            }
        });
        return done.get();
    }

    public static void main(String[] args) throws Exception {
        try (MongoClient client = MongoClients.create("mongodb://127.0.0.1:57017")) {
            MongoCollection<Document> orders = client.getDatabase("matrix").getCollection("reactive_orders");
            await(orders.drop());
            for (int i = 0; i < 20; i++) {
                await(orders.insertOne(new Document("_id", i).append("owner", i % 5)));
            }
            int found = 0;
            for (int i = 0; i < 10; i++) {
                found += await(orders.find(new Document("owner", i % 5))).size();
            }
            System.out.println("found " + found);
            if (found != 40) {
                throw new IllegalStateException("found " + found);
            }
        }
    }
}
