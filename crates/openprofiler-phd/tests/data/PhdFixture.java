import java.util.ArrayList;
import java.util.List;

/**
 * Generates the PHD fixtures on OpenJ9 (see `regenerate.sh`): known object shapes the tests count.
 */
public class PhdFixture {
    static final class Node {
        Node next;
        Object payload;
        int value;

        Node(Node next, Object payload, int value) {
            this.next = next;
            this.payload = payload;
            this.value = value;
        }
    }

    static final class Wide {
        Object a, b, c, d, e, f, g, h, i;
    }

    static Node chain;
    static Object[] sparse = new Object[64];
    static List<Wide> wides = new ArrayList<>();
    static byte[] big = new byte[3 << 20];
    static int[][] grid = new int[8][16];

    public static void main(String[] args) throws Exception {
        Node n = null;
        for (int i = 0; i < 1000; i++) {
            n = new Node(n, i % 10 == 0 ? new long[4] : null, i);
        }
        chain = n;
        for (int i = 0; i < 64; i += 8) {
            sparse[i] = "s" + i;
        }
        for (int i = 0; i < 100; i++) {
            Wide w = new Wide();
            w.a = w;
            w.b = new char[3];
            w.c = "x";
            w.d = w.b;
            w.e = w;
            w.f = w;
            w.g = w;
            w.h = w;
            w.i = new double[2];
            wides.add(w);
        }
        com.ibm.jvm.Dump.heapDumpToFile(args[0]);
    }
}
