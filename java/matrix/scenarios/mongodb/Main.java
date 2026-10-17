package matrix;

import com.mongodb.client.MongoClient;
import com.mongodb.client.MongoClients;
import com.mongodb.client.MongoCollection;
import com.mongodb.client.model.Filters;
import com.mongodb.client.model.Updates;
import org.bson.Document;

/** The synchronous MongoDB driver: inserts, finds (also from a request handler), updates, deletes. */
public class Main {
    public static void main(String[] args) throws Exception {
        try (MongoClient client = MongoClients.create("mongodb://127.0.0.1:57017")) {
            MongoCollection<Document> orders = client.getDatabase("matrix").getCollection("orders");
            orders.drop();
            for (int i = 0; i < 20; i++) {
                orders.insertOne(new Document("_id", i).append("owner", i % 5).append("total", i * 10));
            }
            Requests.serve(10, id -> {
                Document d = orders.find(Filters.eq("owner", Integer.parseInt(id))).first();
                return d == null ? "none" : d.toJson();
            });
            orders.updateMany(Filters.lt("total", 100), Updates.inc("total", 1));
            long n = orders.countDocuments();
            orders.deleteOne(Filters.eq("_id", 0));
            System.out.println("orders " + n);
            if (n != 20) {
                throw new IllegalStateException("orders " + n);
            }
        }
    }
}
