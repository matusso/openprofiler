package dev.openprofiler.boot;

/**
 * Exceptions: every {@code Throwable} constructed, by class and the stack that created it. The
 * duration is not meaningful; counts and stacks are.
 */
final class ExceptionProbe extends Probe {
    ExceptionProbe() {
        super("exceptions", "Exceptions", "Exception class", null);
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {Interception.method("java.lang.Throwable", "<init>", null).exitOnly()};
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (result != null && !call.nested) {
            call.payload = result.getClass().getName();
            Object m = Reflect.call(result, "getMessage");
            if (m != null) {
                call.detail = Reflect.cut(m.toString(), 300);
            }
        }
    }
}
