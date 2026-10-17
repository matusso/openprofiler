package dev.openprofiler.boot;

/**
 * A method a {@link Probe} intercepts: a method of one class, or of every subtype of a class or
 * interface. Names use Java syntax ({@code java.sql.Statement}); the descriptor is a JVM method
 * descriptor, or null for every overload.
 */
public final class Interception {
    static final int SUBTYPES = 1;
    static final int ARGS = 2;
    static final int EXIT_ONLY = 4;
    static final int ENTRY_ONLY = 8;

    final String type;
    final String method;
    final String descriptor;
    int flags;

    private Interception(String type, String method, String descriptor, int flags) {
        this.type = type;
        this.method = method;
        this.descriptor = descriptor;
        this.flags = flags;
    }

    /** {@code method} declared in {@code type} itself. */
    public static Interception method(String type, String method, String descriptor) {
        return new Interception(type, method, descriptor, 0);
    }

    /** {@code method} in every class that extends or implements {@code type} (and in {@code type}). */
    public static Interception subtypes(String type, String method, String descriptor) {
        return new Interception(type, method, descriptor, SUBTYPES);
    }

    /** Passes the arguments (boxed) to {@link Probe#onEnter}. */
    public Interception withArgs() {
        flags |= ARGS;
        return this;
    }

    /** Only the exit is reported (needed for constructors, where {@code this} is unusable at entry). */
    public Interception exitOnly() {
        flags |= EXIT_ONLY;
        return this;
    }

    /**
     * Only the entry is reported: no exit hooks are injected, which makes the interception
     * cheaper. The call is not recorded and must not set a request label.
     */
    public Interception entryOnly() {
        flags |= ENTRY_ONLY;
        return this;
    }
}
