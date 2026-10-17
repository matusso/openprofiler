package matrix;

import com.sun.net.httpserver.HttpServer;
import jakarta.persistence.EntityManager;
import jakarta.persistence.EntityManagerFactory;
import jakarta.persistence.Persistence;
import java.net.InetSocketAddress;
import java.util.List;

/**
 * A JPA provider on H2: persist, find, merge, remove, JPQL queries (also from a request handler)
 * and a bulk update. The SQL the provider generates is recorded by the JDBC probe below the JPA
 * calls.
 */
public class Main {
    static EntityManagerFactory emf;

    public static void main(String[] args) throws Exception {
        emf = Persistence.createEntityManagerFactory("matrix");
        EntityManager em = emf.createEntityManager();
        em.getTransaction().begin();
        for (int i = 1; i <= 20; i++) {
            em.persist(new Owner(i, "owner" + i, i % 2 == 0 ? "Madison" : "Monona"));
        }
        em.flush();
        em.getTransaction().commit();
        em.close();

        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/owners/", ex -> {
            int id = Integer.parseInt(ex.getRequestURI().getPath().substring("/owners/".length()));
            int n = ownersInCityOf(id);
            byte[] body = String.valueOf(n).getBytes();
            ex.sendResponseHeaders(200, body.length);
            ex.getResponseBody().write(body);
            ex.close();
        });
        server.start();
        String base = "http://127.0.0.1:" + server.getAddress().getPort();
        for (int i = 0; i < 30; i++) {
            Http.get(base + "/owners/" + (1 + i % 20));
        }
        server.stop(0);

        em = emf.createEntityManager();
        em.getTransaction().begin();
        Owner o = em.find(Owner.class, 3);
        em.detach(o);
        o.city = "Sun Prairie";
        em.merge(o);
        em.remove(em.find(Owner.class, 4));
        int updated = em.createQuery("UPDATE Owner o SET o.city = 'Verona' WHERE o.id > 15").executeUpdate();
        Owner single = em.createQuery("SELECT o FROM Owner o WHERE o.id = :id", Owner.class)
            .setParameter("id", 5).getSingleResult();
        em.getTransaction().commit();
        em.close();
        System.out.println("updated " + updated + ", single " + single.name);
        emf.close();
    }

    static int ownersInCityOf(int id) {
        EntityManager em = emf.createEntityManager();
        try {
            Owner o = em.find(Owner.class, id);
            List<Owner> same = em.createQuery("SELECT o FROM Owner o WHERE o.city = :city", Owner.class)
                .setParameter("city", o.city).getResultList();
            return same.size();
        } finally {
            em.close();
        }
    }
}
