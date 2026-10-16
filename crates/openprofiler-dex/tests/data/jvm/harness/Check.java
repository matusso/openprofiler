import java.util.Arrays;
import java.util.List;
import modern.Features;

/** Exercises the JVM corpus; the decompiled and recompiled classes must print the same. */
public class Check {
    static void p(Object o) {
        System.out.println(o);
    }

    public static void main(String[] args) {
        Features f = new Features();
        Features.Point pt = new Features.Point(3, -4);
        p(pt + " " + pt.dist() + " " + pt.equals(new Features.Point(3, -4)) + " " + (pt.hashCode() == new Features.Point(3, -4).hashCode()));
        try {
            new Features.Point(-1, 0);
        } catch (IllegalArgumentException e) {
            p("rejected " + e.getMessage());
        }
        for (int d = 0; d < 9; d++) p(f.day(d));
        p(f.score("A") + " " + f.score("B") + " " + f.score("Cc"));
        p(f.area(new Features.Circle(2)) + " " + f.area(new Features.Square(3)) + " " + f.area(new Features.Square(-1)));
        p(f.text());
        p(f.streams(List.of(1, 2, 3, 4, 5, 6, 10)));
        p(f.grouping(List.of("a", "bb", "cc", "ddd", "e")));
        p(f.captures(5) + " " + f.captures(-2));
        f.nested("m").run();
        p(f.log());
        p(f.bits(0x1234L, 3) + " " + f.bits(-7L, 1));
        p(f.resources("a", "b"));
        p(f.resources("bad", "c"));
        p(f.finallyFlow(5) + " " + f.finallyFlow(0) + " " + f.finallyFlow(-3));
        p(f.sync(new Object()));
        p(f.first(List.of("  ", " x ", "y")) + " " + f.first(List.of()));
        p(f.op("ADD", 3, 4) + " " + f.op("MUL", 3, 4));
        p(new String(f.chars("hello")));
        p(f.ternaries(1, 7, false) + " " + f.ternaries(-1, null, true) + " " + f.ternaries(0, 2, false));
        p(f.labeled(new int[][] {{1, 2}, {-1, 50}, {3, 200, 4}, {9}}));
        p(f.inner());
        p(Features.max(Arrays.asList(3, 9, 2)) + " " + Features.max(List.<String>of()));
        p(f.local(4));
        p(f.concat('z', 1L << 33, 0.25, null, new int[3]));
        p(f.boxes(1000, 1000, List.of(7, 8, 7, 9)) + " | " + f.boxes(5, 5, List.of(1, 2)));
        p(f.collide("Aa") + " " + f.collide("BB") + " " + f.collide("C") + " " + f.collide("xyz"));
        p(f.loopFinally(5) + " " + f.loopFinally(1));
        p(f.nestedFinally("abc") + " " + f.nestedFinally(null));
        try {
            p(f.nullableResource(true) + " " + f.nullableResource(false));
        } catch (java.io.IOException e) {
            p("io " + e);
        }
        p(f.index(List.of("a", "b", "a")));
    }
}
