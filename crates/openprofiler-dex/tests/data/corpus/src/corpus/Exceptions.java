package corpus;

import java.io.Closeable;
import java.io.IOException;

public class Exceptions {
    private final Object lock = new Object();
    private int state;

    public int parse(String s) {
        try {
            return Integer.parseInt(s);
        } catch (NumberFormatException e) {
            return -1;
        }
    }

    public String multi(Object o) {
        try {
            return o.toString().substring(3);
        } catch (NullPointerException | StringIndexOutOfBoundsException e) {
            return "bad: " + e.getMessage();
        }
    }

    public void check(int v) throws IOException {
        if (v < 0) {
            throw new IllegalArgumentException("negative: " + v);
        }
        if (v == 0) {
            throw new IOException("zero");
        }
    }

    public int withFinally(String s) {
        int r = 0;
        try {
            r = s.length();
        } finally {
            state++;
        }
        return r;
    }

    public void close(Closeable c) {
        try {
            c.close();
        } catch (IOException e) {
            state = -1;
        }
    }

    public void increment() {
        synchronized (lock) {
            state++;
        }
    }

    public synchronized int getState() {
        return state;
    }

    public int nested(String a, String b) {
        try {
            try {
                return Integer.parseInt(a);
            } catch (NumberFormatException e) {
                return Integer.parseInt(b);
            }
        } catch (RuntimeException e) {
            return 0;
        }
    }
}
