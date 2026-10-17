package matrix;

import jakarta.persistence.Entity;
import jakarta.persistence.Id;

@Entity
public class Owner {
    @Id
    public int id;
    public String name;
    public String city;

    public Owner() {}

    Owner(int id, String name, String city) {
        this.id = id;
        this.name = name;
        this.city = city;
    }
}
