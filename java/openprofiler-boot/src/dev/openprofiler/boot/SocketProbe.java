package dev.openprofiler.boot;

/** Sockets: connects and accepts (timed, by address) and open sockets as control objects. */
final class SocketProbe extends Probe {
    private static final int CONNECT = 0, ACCEPT = 1, CLOSE = 2;

    SocketProbe() {
        super("sockets", "Sockets", "Address", "Socket");
    }

    @Override
    protected Interception[] interceptions() {
        return new Interception[] {
            Interception.method("java.net.Socket", "connect", "(Ljava/net/SocketAddress;I)V").withArgs(),
            Interception.method("java.net.ServerSocket", "accept", "()Ljava/net/Socket;"),
            Interception.method("java.net.Socket", "close", "()V").entryOnly(),
        };
    }

    @Override
    protected void onEnter(Call call, Object self, Object[] args) {
        switch (call.interception) {
            case CONNECT:
                call.state = args != null && args.length > 0 ? String.valueOf(args[0]) : "?";
                call.payload = "connect " + call.state;
                break;
            case ACCEPT:
                call.payload = "accept " + Reflect.str(self, "getLocalSocketAddress");
                break;
            case CLOSE:
                closeControl(self);
                break;
            default:
                break;
        }
    }

    @Override
    protected void onExit(Call call, Object self, Object result, Throwable error) {
        if (error != null) {
            return;
        }
        if (call.interception == CONNECT) {
            call.control = openControl(self, "to " + call.state);
        } else if (call.interception == ACCEPT && result != null) {
            call.control = openControl(result, "from " + Reflect.str(result, "getRemoteSocketAddress"));
        }
    }
}
