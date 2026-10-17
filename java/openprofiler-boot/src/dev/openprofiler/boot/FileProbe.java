package dev.openprofiler.boot;

import java.io.File;

/**
 * Files opened through {@code FileInputStream}, {@code FileOutputStream} and
 * {@code RandomAccessFile}: open streams as control objects, and bulk reads and writes by path.
 */
final class FileProbe extends Probe {
    private static final int OPEN_IN = 0, OPEN_OUT = 1, OPEN_RAF = 2, CLOSE_IN = 3, CLOSE_OUT = 4, CLOSE_RAF = 5,
        READ_IN = 6, WRITE_OUT = 7, READ_RAF = 8, WRITE_RAF = 9;

    FileProbe() {
        super("files", "Files", "File operation", "File");
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.method("java.io.FileInputStream", "<init>", "(Ljava/io/File;)V").withArgs(),
            Interception.method("java.io.FileOutputStream", "<init>", "(Ljava/io/File;Z)V").withArgs(),
            Interception.method("java.io.RandomAccessFile", "<init>", "(Ljava/io/File;Ljava/lang/String;)V").withArgs(),
            Interception.method("java.io.FileInputStream", "close", "()V"),
            Interception.method("java.io.FileOutputStream", "close", "()V"),
            Interception.method("java.io.RandomAccessFile", "close", "()V"),
            Interception.method("java.io.FileInputStream", "read", "([BII)I").withArgs(),
            Interception.method("java.io.FileOutputStream", "write", "([BII)V").withArgs(),
            Interception.method("java.io.RandomAccessFile", "read", "([BII)I").withArgs(),
            Interception.method("java.io.RandomAccessFile", "write", "([BII)V").withArgs(),
        };
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        switch (call.interception) {
            case OPEN_IN:
            case OPEN_OUT:
            case OPEN_RAF:
                // Constructor entry (no receiver yet): remember the path for the exit.
                call.state = args != null && args.length > 0 && args[0] instanceof File ? ((File) args[0]).getPath() : null;
                break;
            case CLOSE_IN:
            case CLOSE_OUT:
            case CLOSE_RAF:
                closeControl(self);
                break;
            case READ_IN:
            case READ_RAF:
            case WRITE_OUT:
            case WRITE_RAF: {
                String path = controlDescription(self);
                if (path != null && !call.nested) {
                    boolean read = call.interception == READ_IN || call.interception == READ_RAF;
                    call.payload = (read ? "read " : "write ") + path;
                    call.control = controlId(self);
                }
                break;
            }
            default:
                break;
        }
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        int i = call.interception;
        if (i == OPEN_IN || i == OPEN_OUT || i == OPEN_RAF) {
            // For constructors `self` is the new object.
            if (error == null && self != null && call.state != null) {
                openControl(self, (String) call.state);
            }
        } else if (call.payload != null && result instanceof Integer && (Integer) result > 0) {
            call.detail = result + " bytes";
        }
    }
}
