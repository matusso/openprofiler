package modern;

import java.io.Closeable;
import java.io.IOException;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.TreeMap;
import java.util.function.BiFunction;
import java.util.function.Function;
import java.util.function.IntBinaryOperator;
import java.util.function.Supplier;
import java.util.stream.Collectors;

/** Language features of recent Java versions, as javac compiles them. */
public class Features {
    public record Point(int x, int y) {
        public Point {
            if (x < 0) {
                throw new IllegalArgumentException("x < 0");
            }
        }

        public int dist() {
            return Math.abs(x) + Math.abs(y);
        }
    }

    public sealed interface Shape permits Circle, Square {}

    public static final class Circle implements Shape {
        final double r;

        public Circle(double r) {
            this.r = r;
        }
    }

    public static final class Square implements Shape {
        final double side;

        public Square(double side) {
            this.side = side;
        }
    }

    enum Op {
        ADD {
            int apply(int a, int b) {
                return a + b;
            }
        },
        MUL {
            int apply(int a, int b) {
                return a * b;
            }
        };

        abstract int apply(int a, int b);
    }

    private final List<String> log = new ArrayList<>();
    private int counter;
    private long total = 1L << 40;
    private final int[] cells = new int[4];

    public String day(int d) {
        return switch (d) {
            case 1, 7 -> "weekend";
            case 2, 3, 4, 5, 6 -> {
                String s = "week";
                yield s + "day";
            }
            default -> "unknown";
        };
    }

    public int score(String grade) {
        int bonus = switch (grade) {
            case "A" -> 10;
            case "B" -> 5;
            default -> 0;
        };
        return bonus + grade.length();
    }

    public double area(Shape s) {
        if (s instanceof Circle c) {
            return Math.PI * c.r * c.r;
        }
        if (s instanceof Square q && q.side > 0) {
            return q.side * q.side;
        }
        return 0;
    }

    public String text() {
        return """
            Hello,
              "World"
            """;
    }

    public List<String> streams(List<Integer> in) {
        return in.stream()
                .filter(x -> x % 2 == 0)
                .map(x -> "n" + x)
                .sorted(Comparator.reverseOrder())
                .collect(Collectors.toList());
    }

    public Map<Integer, Long> grouping(List<String> words) {
        return words.stream().collect(Collectors.groupingBy(String::length, TreeMap::new, Collectors.counting()));
    }

    public int captures(int base) {
        int offset = base * 2;
        IntBinaryOperator op = (a, b) -> a * b + offset + counter;
        Function<Integer, Integer> twice = x -> op.applyAsInt(x, 2);
        Supplier<List<String>> make = ArrayList::new;
        List<String> l = make.get();
        l.add("x");
        BiFunction<String, Integer, String> rep = String::repeat;
        return twice.apply(base) + l.size() + rep.apply("ab", 2).length();
    }

    public Runnable nested(String msg) {
        return () -> {
            for (int i = 0; i < 2; i++) {
                int k = i;
                Runnable inner = () -> log.add(msg + k);
                inner.run();
            }
        };
    }

    public List<String> log() {
        return log;
    }

    public long bits(long v, int shift) {
        total ^= v;
        total >>>= shift;
        counter += (int) (v & 0xff);
        cells[counter & 3] += 3;
        cells[1]++;
        long r = total;
        r |= ~v;
        return r + cells[1] + (counter > 10 ? 1 : 0);
    }

    static final class Res implements Closeable {
        final List<String> out;
        final String name;

        Res(List<String> out, String name) {
            this.out = out;
            this.name = name;
            out.add("open " + name);
        }

        @Override
        public void close() throws IOException {
            out.add("close " + name);
            if (name.equals("bad")) {
                throw new IOException("close failed");
            }
        }
    }

    public List<String> resources(String a, String b) {
        List<String> out = new ArrayList<>();
        try (Res r1 = new Res(out, a); Res r2 = new Res(out, b)) {
            out.add("body " + r1.name + r2.name);
        } catch (IOException e) {
            out.add("caught " + e.getMessage());
        } finally {
            out.add("finally");
        }
        return out;
    }

    public int finallyFlow(int x) {
        int r = 0;
        try {
            if (x < 0) {
                return -1;
            }
            r = 100 / x;
        } catch (ArithmeticException e) {
            r = -2;
        } finally {
            counter++;
        }
        return r;
    }

    public synchronized int sync(Object lock) {
        synchronized (lock) {
            counter += 2;
        }
        return counter;
    }

    public Optional<String> first(List<String> xs) {
        for (var s : xs) {
            if (!s.isBlank()) {
                return Optional.of(s.strip());
            }
        }
        return Optional.empty();
    }

    public int op(String name, int a, int b) {
        return Op.valueOf(name).apply(a, b);
    }

    public char[] chars(String s) {
        char[] c = s.toCharArray();
        for (int i = 0, j = c.length - 1; i < j; i++, j--) {
            char t = c[i];
            c[i] = c[j];
            c[j] = t;
        }
        return c;
    }

    public String ternaries(int a, Integer boxed, boolean flag) {
        String s = a > 0 ? "pos" : a < 0 ? "neg" : "zero";
        int u = boxed != null ? boxed : -1;
        boolean f = !flag && a != 0;
        return s + u + f;
    }

    public int labeled(int[][] m) {
        int found = 0;
        outer:
        for (int[] row : m) {
            for (int v : row) {
                if (v < 0) {
                    continue outer;
                }
                if (v > 100) {
                    break outer;
                }
                found += v;
            }
        }
        return found;
    }

    class Inner {
        int bump() {
            return ++counter;
        }
    }

    public int inner() {
        Inner i = new Inner();
        i.bump();
        return i.bump();
    }

    public static <T extends Comparable<? super T>> T max(List<? extends T> xs) {
        T best = null;
        for (T x : xs) {
            if (best == null || x.compareTo(best) > 0) {
                best = x;
            }
        }
        return best;
    }

    public String local(int n) {
        class Counter {
            int c;

            void add(int k) {
                c += k * n;
            }
        }
        Counter c = new Counter();
        for (int i = 0; i < 3; i++) {
            c.add(i);
        }
        Object o = new Object() {
            @Override
            public String toString() {
                return "anon" + c.c;
            }
        };
        return o.toString();
    }

    public String concat(char c, long l, double d, Object o, int[] arr) {
        return "c=" + c + " l=" + l + " d=" + d + " o=" + o + " len=" + arr.length + '!';
    }
}
