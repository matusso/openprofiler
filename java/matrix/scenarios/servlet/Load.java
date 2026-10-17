package matrix;

/** The requests every servlet container scenario sends. */
final class Load {
    static final int OWNERS = 40, FAILS = 5;

    private Load() {}

    static void run(String base) throws Exception {
        Http.await(base + "/ping");
        for (int i = 0; i < OWNERS; i++) {
            int s = Http.get(base + "/owners/" + (1 + i % 7) + "?details=true");
            if (s != 200) {
                throw new IllegalStateException("status " + s);
            }
        }
        for (int i = 0; i < FAILS; i++) {
            Http.get(base + "/fail");
        }
        for (int i = 0; i < 10; i++) {
            Http.get(base + "/async");
        }
        OwnerServlet.POOL.shutdown();
    }
}
