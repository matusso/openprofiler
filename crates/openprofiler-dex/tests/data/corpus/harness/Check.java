import corpus.Basics;
import corpus.Control;
import corpus.Exceptions;
import corpus.Shapes;
import java.io.Closeable;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Exercises the corpus; the decompiled and recompiled classes must print the same. */
public class Check {
    static void p(Object o) {
        System.out.println(o);
    }

    public static void main(String[] args) throws Exception {
        Basics b = new Basics();
        p(b.arithmetic(17, 5) + " " + b.arithmetic(-3, 9));
        p(b.mixed(12L, 2.5f, 7.25) + " " + b.mixed(3L, -1f, 0.5));
        p(b.isBig(41) + " " + b.isBig(43));
        p(b.describe("hello") + " | " + b.describe(42));
        p(Arrays.toString(b.squares(5)));
        p(Arrays.deepToString(b.grid(2, 3)));
        p(Arrays.toString(b.literals()));
        p(b.nextChar('a') + " " + b.narrow(300));
        b.add("x");
        b.add("yz");
        p(b.sum(1, 2, 3, 4) + " " + b.tableSum() + " " + Basics.counter() + " " + b.getRatio());
        p(b.joined());

        Control c = new Control();
        for (int x : new int[] {-5, 0, 3, 99}) p(c.classify(x));
        p(c.both("a", "a") + " " + c.both("a", null) + " " + c.both(null, "b") + " " + c.both("a", "b"));
        p(c.either(1, -1) + " " + c.either(-1, 1) + " " + c.either(0, 0));
        p(c.loops(10) + " " + c.loops(200) + " " + c.loops(0));
        p(c.find(new int[][] {{1, 2}, {3, 4}}, 4) + " " + c.find(new int[][] {{1}}, 9));
        for (int d = -1; d < 5; d++) p(c.dense(d));
        for (int d : new int[] {10, 1000, 100000, 7}) p(c.sparse(d));
        p(c.strings("alpha") + " " + c.strings("beta") + " " + c.strings("gamma"));
        List<String> items = new ArrayList<>(Arrays.asList("", "a", "", "b"));
        p(c.count(items) + " " + items);
        p(c.max(3, 9) + " " + c.max(9, 3));
        p(c.firstNonEmpty(new String[] {null, "", "z", "w"}) + " " + c.firstNonEmpty(new String[] {}));

        Exceptions e = new Exceptions();
        p(e.parse("12") + " " + e.parse("x"));
        p(e.multi("abcdef") + " | " + e.multi("ab") + " | " + e.multi(null));
        for (int v : new int[] {5, 0, -2}) {
            try {
                e.check(v);
                p("ok");
            } catch (Exception ex) {
                p(ex.getClass().getSimpleName() + ": " + ex.getMessage());
            }
        }
        p(e.withFinally("abc") + " " + e.getState());
        try {
            e.withFinally(null);
        } catch (NullPointerException ex) {
            p("npe " + e.getState());
        }
        e.close(() -> {
            throw new java.io.IOException("boom");
        });
        Closeable ok = () -> {};
        e.close(ok);
        p(e.getState());
        e.increment();
        e.increment();
        p(e.getState());
        p(e.nested("1", "2") + " " + e.nested("x", "3") + " " + e.nested("x", "y"));

        Shapes s = new Shapes() {
            @Override
            public Kind kind() {
                return Kind.SQUARE;
            }
        };
        List<Shapes.Shape> made = s.make(3);
        StringBuilder areas = new StringBuilder();
        for (Shapes.Shape sh : made) areas.append(String.format("%.3f ", sh.area()));
        p(areas.toString().trim());
        p(made.get(1).label().startsWith("Circle"));
        p(s.kindIndex(Shapes.Kind.CIRCLE) + " " + s.kindIndex(s.kind()) + " " + Shapes.Kind.SQUARE.code());
        s.task("run ").run();
        Shapes.Registry r = s.new Registry();
        r.put("one", made.get(0));
        p(r.get("one") == made.get(0));
        p(Shapes.maxOf(Arrays.asList(3, 9, 2)) + " " + Shapes.maxOf(Arrays.asList("b", "c", "a")));
    }
}
