// Workload for scripts/e2e-remote.sh: one thread burning CPU in a recognizable method.
public class Busy {
    static volatile long sink;

    static long spinBusyLoop(long n) {
        long h = n;
        for (int i = 0; i < 1_000_000; i++) {
            h = h * 31 + i ^ (h >>> 7);
        }
        return h;
    }

    public static void main(String[] args) {
        System.out.println("busy");
        for (long n = 0; ; n++) {
            sink += spinBusyLoop(n);
        }
    }
}
