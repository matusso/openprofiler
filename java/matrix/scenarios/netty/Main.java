package matrix;

import dev.openprofiler.api.OpenProfiler;
import io.netty.bootstrap.ServerBootstrap;
import io.netty.buffer.Unpooled;
import io.netty.channel.Channel;
import io.netty.channel.ChannelFutureListener;
import io.netty.channel.ChannelHandlerContext;
import io.netty.channel.ChannelInitializer;
import io.netty.channel.EventLoopGroup;
import io.netty.channel.SimpleChannelInboundHandler;
import io.netty.channel.nio.NioEventLoopGroup;
import io.netty.channel.socket.SocketChannel;
import io.netty.channel.socket.nio.NioServerSocketChannel;
import io.netty.handler.codec.http.DefaultFullHttpResponse;
import io.netty.handler.codec.http.FullHttpRequest;
import io.netty.handler.codec.http.FullHttpResponse;
import io.netty.handler.codec.http.HttpHeaderNames;
import io.netty.handler.codec.http.HttpObjectAggregator;
import io.netty.handler.codec.http.HttpResponseStatus;
import io.netty.handler.codec.http.HttpServerCodec;
import io.netty.handler.codec.http.HttpVersion;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/** A plain Netty HTTP server: codec, aggregator and a {@code SimpleChannelInboundHandler}. */
public class Main {
    static final ExecutorService POOL = Executors.newFixedThreadPool(2);

    static final class Handler extends SimpleChannelInboundHandler<FullHttpRequest> {
        @Override
        protected void channelRead0(ChannelHandlerContext ctx, FullHttpRequest req) throws Exception {
            POOL.submit(() -> OpenProfiler.probeEvent("work", "load owner", 1000)).get();
            boolean fail = req.uri().startsWith("/fail");
            FullHttpResponse resp = new DefaultFullHttpResponse(HttpVersion.HTTP_1_1,
                fail ? HttpResponseStatus.INTERNAL_SERVER_ERROR : HttpResponseStatus.OK,
                Unpooled.copiedBuffer(render(req.uri()), StandardCharsets.UTF_8));
            resp.headers().set(HttpHeaderNames.CONTENT_LENGTH, resp.content().readableBytes());
            ctx.writeAndFlush(resp).addListener(ChannelFutureListener.CLOSE);
        }
    }

    static String render(String uri) {
        return "owner " + uri;
    }

    @SuppressWarnings("deprecation")
    public static void main(String[] args) throws Exception {
        EventLoopGroup group = new NioEventLoopGroup(2);
        Channel ch = new ServerBootstrap().group(group).channel(NioServerSocketChannel.class)
            .childHandler(new ChannelInitializer<SocketChannel>() {
                @Override
                protected void initChannel(SocketChannel c) {
                    c.pipeline().addLast(new HttpServerCodec(), new HttpObjectAggregator(65536), new Handler());
                }
            })
            .bind("127.0.0.1", 0).sync().channel();
        String base = "http://127.0.0.1:" + ((InetSocketAddress) ch.localAddress()).getPort();
        Http.await(base + "/ping");
        for (int i = 0; i < 30; i++) {
            Http.get(base + "/owners/" + (1 + i % 5) + "?x=1");
        }
        for (int i = 0; i < 3; i++) {
            Http.get(base + "/fail");
        }
        ch.close().sync();
        group.shutdownGracefully().sync();
        POOL.shutdown();
    }
}
