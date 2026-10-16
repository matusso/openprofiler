package corpus;

import java.util.ArrayList;
import java.util.List;

public class Basics {
    public static final int LIMIT = 42;
    public static final String NAME = "basics";
    private static int counter;
    private static final long[] TABLE = {1L, 2L, 3L};
    private final List<String> names = new ArrayList<>();
    private int total;
    private double ratio = 0.5;
    protected char marker = 'x';
    boolean enabled;

    public Basics() {
        this(LIMIT);
    }

    public Basics(int total) {
        this.total = total;
        counter++;
    }

    public int arithmetic(int a, int b) {
        int c = a * b + (a - b) / 3;
        c %= 7;
        c <<= 2;
        c = c >>> 1 ^ (a & b | 0xff);
        return -c + ~a;
    }

    public long mixed(long x, float f, double d) {
        long y = x * 1000L + (long) f;
        double z = d / 2.0 + f;
        return y + (long) z + (x > 10 ? 1 : 2);
    }

    public boolean isBig(int v) {
        return v > LIMIT;
    }

    public String describe(Object o) {
        if (o instanceof String) {
            String s = (String) o;
            return "string of " + s.length() + " chars";
        }
        return "object " + o + " #" + total;
    }

    public int[] squares(int n) {
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            out[i] = i * i;
        }
        return out;
    }

    public int[][] grid(int w, int h) {
        int[][] g = new int[h][w];
        g[0][0] = 1;
        return g;
    }

    public String[] literals() {
        return new String[] {"a", "b", NAME};
    }

    public char nextChar(char c) {
        return (char) (c + 1);
    }

    public byte narrow(int v) {
        return (byte) v;
    }

    public void add(String name) {
        names.add(name);
        total += name.length();
        enabled = !enabled;
    }

    public int sum(int... values) {
        int s = 0;
        for (int v : values) {
            s += v;
        }
        return s;
    }

    public long tableSum() {
        long s = 0;
        for (long v : TABLE) {
            s += v;
        }
        return s;
    }

    public static int counter() {
        return counter;
    }

    public double getRatio() {
        return ratio;
    }

    public String joined() {
        StringBuilder sb = new StringBuilder();
        for (String n : names) {
            if (sb.length() > 0) {
                sb.append(", ");
            }
            sb.append(n);
        }
        return sb.toString();
    }
}
