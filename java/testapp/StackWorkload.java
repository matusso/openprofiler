import java.util.Arrays;

/**
 * Workload for the stack-walk quality test: Java threads that spend much of their time where
 * AsyncGetCallTrace tends to fail (array copy/fill stubs, itable/vtable dispatch, runtime calls
 * from compiled code, allocation slow paths), plus enough garbage on a small heap to keep the GC
 * threads busy. Usage: java StackWorkload.java [seconds] (0 = run until killed).
 */
public class StackWorkload {
    static volatile long sink;

    interface Shape { double area(); }
    static class Square implements Shape {
        final double s;
        Square(double s) { this.s = s; }
        public double area() { return s * s; }
    }
    static class Circle implements Shape {
        final double r;
        Circle(double r) { this.r = r; }
        public double area() { return Math.PI * r * r; }
    }
    static class Rect implements Shape {
        final double w, h;
        Rect(double w, double h) { this.w = w; this.h = h; }
        public double area() { return w * h; }
    }
    static class Tri implements Shape {
        final double b, h;
        Tri(double b, double h) { this.b = b; this.h = h; }
        public double area() { return b * h / 2; }
    }

    static void copy() {
        long[] a = new long[1 << 16];
        long[] b = new long[1 << 16];
        for (int i = 0; i < 200; i++) {
            Arrays.fill(a, i);
            System.arraycopy(a, 0, b, 0, a.length);
            sink += b[i] + a.clone()[i];
        }
    }

    static void dispatch() {
        Shape[] shapes = { new Square(2), new Circle(1), new Rect(2, 3), new Tri(3, 4) };
        double acc = 0;
        for (int i = 0; i < 2_000_000; i++) {
            acc += shapes[i & 3].area();
        }
        sink += (long) acc;
    }

    static void math() {
        double acc = 0;
        for (int i = 0; i < 500_000; i++) {
            acc += Math.sin(i) * Math.log(i + 1) + Math.pow(i, 0.3);
        }
        sink += (long) acc;
    }

    static void allocate() {
        for (int i = 0; i < 2000; i++) {
            Object[] o = new Object[64 + (i & 63)];
            o[0] = new StringBuilder().append(i).append('-').append(sink).toString();
            sink += o.length;
        }
    }

    static void spin(String name, Runnable r) {
        Thread t = new Thread(() -> { while (true) r.run(); }, name);
        t.setDaemon(true);
        t.start();
    }

    public static void main(String[] args) throws Exception {
        long seconds = args.length > 0 ? Long.parseLong(args[0]) : 0;
        System.out.println("PID " + ProcessHandle.current().pid());
        System.out.flush();
        spin("stack-copy", StackWorkload::copy);
        spin("stack-dispatch", StackWorkload::dispatch);
        spin("stack-math", StackWorkload::math);
        spin("stack-alloc", StackWorkload::allocate);
        long end = System.currentTimeMillis() + seconds * 1000;
        while (seconds == 0 || System.currentTimeMillis() < end) {
            Thread.sleep(100);
        }
        System.out.println("DONE");
    }
}
