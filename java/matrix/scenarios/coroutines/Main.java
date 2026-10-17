package matrix;

/** Requests handled with Kotlin coroutines ({@link Coroutines}). */
public class Main {
    public static void main(String[] args) throws Exception {
        Requests.serve(20, Coroutines::handle);
    }
}
