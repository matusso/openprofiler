package corpus;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public abstract class Shapes {
    public interface Shape {
        double area();

        default String label() {
            return getClass().getSimpleName() + " " + area();
        }
    }

    public enum Kind {
        CIRCLE("c"),
        SQUARE("s");

        private final String code;

        Kind(String code) {
            this.code = code;
        }

        public String code() {
            return code;
        }
    }

    public static class Circle implements Shape {
        private final double r;

        public Circle(double r) {
            this.r = r;
        }

        @Override
        public double area() {
            return Math.PI * r * r;
        }
    }

    public class Registry {
        private final Map<String, Shape> byName = new HashMap<>();

        public void put(String name, Shape s) {
            byName.put(name, s);
            registered++;
        }

        public Shape get(String name) {
            return byName.get(name);
        }
    }

    protected int registered;

    public abstract Kind kind();

    public List<Shape> make(int n) {
        List<Shape> out = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            out.add(new Circle(i));
        }
        out.add(new Shape() {
            @Override
            public double area() {
                return registered;
            }
        });
        return out;
    }

    public Runnable task(final String msg) {
        return () -> System.out.println(msg + registered);
    }

    public int kindIndex(Kind k) {
        switch (k) {
            case CIRCLE:
                return 1;
            case SQUARE:
                return 2;
            default:
                return 0;
        }
    }

    public static <T extends Comparable<T>> T maxOf(List<T> items) {
        T best = null;
        for (T t : items) {
            if (best == null || t.compareTo(best) > 0) {
                best = t;
            }
        }
        return best;
    }
}
