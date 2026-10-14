package dev.openprofiler.intellij

import com.google.gson.Gson
import com.google.gson.JsonElement
import com.google.gson.JsonObject
import com.google.gson.JsonParser
import com.intellij.notification.NotificationGroupManager
import com.intellij.notification.NotificationType
import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationInfo
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.service
import com.intellij.openapi.diagnostic.thisLogger
import com.intellij.openapi.project.Project
import com.intellij.util.concurrency.AppExecutorUtil
import com.intellij.util.messages.Topic
import java.net.http.HttpClient
import java.net.http.WebSocket
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CompletionStage
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/** Daemon connection state and heat changes, published on the application message bus. */
interface DaemonListener {
    fun connectionChanged(connected: Boolean) {}

    /** Profile data may have changed; refresh the CPU heat. */
    fun heatChanged() {}

    companion object {
        @Topic.AppLevel
        val TOPIC = Topic.create("OpenProfiler daemon", DaemonListener::class.java)
    }
}

/** A thin JSON-RPC client of the daemon's IDE bridge WebSocket. Reconnects in the background. */
@Service(Service.Level.APP)
class DaemonClient : Disposable {
    private val gson = Gson()
    private val http = HttpClient.newHttpClient()
    private val nextId = AtomicInteger(1)
    private val pending = ConcurrentHashMap<Int, CompletableFuture<JsonElement>>()
    private val scheduler = AppExecutorUtil.getAppScheduledExecutorService()

    @Volatile private var socket: WebSocket? = null
    @Volatile private var retry: ScheduledFuture<*>? = null
    private val connecting = AtomicBoolean()
    @Volatile private var liveRefresh: ScheduledFuture<*>? = null
    private val profiledRuns = AtomicInteger()

    /** The agent the daemon uses, fetched on connect. */
    @Volatile var daemonAgent: AgentLaunch? = null
        private set

    @Volatile var url: String? = null
        private set

    val isConnected: Boolean get() = socket != null

    /** Connects unless already connected; failures retry quietly. */
    fun ensureConnected() {
        if (socket == null && retry == null) connect(quiet = true)
    }

    fun reconnect() {
        retry?.cancel(false)
        retry = null
        socket?.let {
            socket = null
            it.abort()
            publish { l -> l.connectionChanged(false) }
        }
        connect(quiet = false)
    }

    /** Connects to the daemon. With [quiet], failures only schedule a retry instead of showing a notification. */
    fun connect(quiet: Boolean): CompletableFuture<Unit> {
        if (!connecting.compareAndSet(false, true)) return CompletableFuture.completedFuture(Unit)
        return doConnect(quiet).whenComplete { _, _ -> connecting.set(false) }
    }

    private fun doConnect(quiet: Boolean): CompletableFuture<Unit> = CompletableFuture.supplyAsync({
        OpenProfilerSettings.getInstance().discovery()
    }, AppExecutorUtil.getAppExecutorService()).thenCompose { d ->
        if (d == null) {
            CompletableFuture.failedFuture(IllegalStateException("daemon not found"))
        } else {
            http.newWebSocketBuilder().buildAsync(d.ideUri(), Listener()).thenCompose { ws ->
                socket = ws
                url = d.url
                call("hello", mapOf("ide" to "intellij", "version" to ApplicationInfo.getInstance().fullVersion))
            }.thenCompose {
                // No agent next to the daemon is fine: launching then needs the agent path setting.
                call("agent", emptyMap<String, Any>()).handle { agent, _ ->
                    daemonAgent = agent?.let { gson.fromJson(it, AgentLaunch::class.java) }
                }
            }
        }
    }.handle { _, err ->
        if (err == null) {
            retry = null
            publish { it.connectionChanged(true) }
        } else {
            socket?.abort()
            socket = null
            if (!quiet) {
                notify(
                    "OpenProfiler daemon not found. Start <code>openprofiler daemon</code> or set the URL and token in Settings | Tools | OpenProfiler.",
                    NotificationType.WARNING,
                )
            }
            scheduleRetry()
        }
    }

    private fun scheduleRetry() {
        if (retry?.isDone == false) return
        retry = scheduler.schedule({ retry = null; connect(quiet = true) }, 10, TimeUnit.SECONDS)
    }

    /** Sends a request; the future completes with the `result` or fails with the daemon's error message. */
    fun call(method: String, params: Any): CompletableFuture<JsonElement> {
        val ws = socket ?: return CompletableFuture.failedFuture(IllegalStateException("not connected to the OpenProfiler daemon"))
        val id = nextId.getAndIncrement()
        val f = CompletableFuture<JsonElement>()
        pending[id] = f
        val msg = mapOf("jsonrpc" to "2.0", "id" to id, "method" to method, "params" to params)
        ws.sendText(gson.toJson(msg), true).whenComplete { _, err ->
            if (err != null) pending.remove(id)?.completeExceptionally(err)
        }
        return f
    }

    fun <T> call(method: String, params: Any, type: Class<T>): CompletableFuture<T> =
        call(method, params).thenApply { gson.fromJson(it, type) }

    /** The agent from the settings, else the one the daemon reported. */
    fun agent(): AgentLaunch? {
        val configured = OpenProfilerSettings.getInstance().state.agentPath.orEmpty()
        return if (configured.isNotEmpty()) AgentLaunch(configured, "tls") else daemonAgent
    }

    /** Asks the daemon to connect to a JVM launched with `infofile=<path>`; refreshes the heat while it runs. */
    fun attachLaunched(project: Project, infofile: String, name: String): CompletableFuture<Unit> =
        call("attachLaunched", mapOf("infofile" to infofile)).handle { session, err ->
            if (err != null) {
                notify("Could not connect to $name: ${err.cause?.message ?: err.message}", NotificationType.WARNING, project)
                throw err
            }
            notify("Profiling ${session.asJsonObject["name"]?.asString ?: name}", NotificationType.INFORMATION, project)
            if (profiledRuns.getAndIncrement() == 0) {
                liveRefresh = scheduler.scheduleWithFixedDelay({ publish { it.heatChanged() } }, 5, 5, TimeUnit.SECONDS)
            }
        }

    /** A profiled run ended. */
    fun profiledRunEnded() {
        if (profiledRuns.decrementAndGet() > 0) return
        liveRefresh?.cancel(false)
        liveRefresh = null
        publish { it.heatChanged() }
    }

    private fun onMessage(text: String) {
        val msg = runCatching { JsonParser.parseString(text).asJsonObject }.getOrNull() ?: return
        val id = msg["id"]?.takeIf { it.isJsonPrimitive }?.asInt
        if (id != null) {
            val f = pending.remove(id) ?: return
            val error = msg["error"]?.takeIf { it.isJsonObject }?.asJsonObject
            if (error != null) f.completeExceptionally(RuntimeException(error["message"]?.asString ?: "daemon error"))
            else f.complete(msg["result"])
        } else if (msg["method"]?.asString == "navigate") {
            val p = msg["params"] as? JsonObject ?: return
            val cls = p["class"]?.asString ?: return
            Navigator.navigate(cls, p["method"]?.takeIf { !it.isJsonNull }?.asString, p["line"]?.takeIf { !it.isJsonNull }?.asInt)
        }
    }

    private fun onClosed(ws: WebSocket) {
        if (socket !== ws) return
        socket = null
        val gone = IllegalStateException("disconnected from the OpenProfiler daemon")
        pending.values.forEach { it.completeExceptionally(gone) }
        pending.clear()
        publish { it.connectionChanged(false) }
        scheduleRetry()
    }

    private inner class Listener : WebSocket.Listener {
        private val buf = StringBuilder()

        override fun onText(ws: WebSocket, data: CharSequence, last: Boolean): CompletionStage<*>? {
            buf.append(data)
            if (last) {
                val text = buf.toString()
                buf.setLength(0)
                try {
                    onMessage(text)
                } catch (e: Exception) {
                    thisLogger().warn("bad message from the OpenProfiler daemon", e)
                }
            }
            ws.request(1)
            return null
        }

        override fun onClose(ws: WebSocket, statusCode: Int, reason: String): CompletionStage<*>? {
            onClosed(ws)
            return null
        }

        override fun onError(ws: WebSocket, error: Throwable) {
            onClosed(ws)
        }
    }

    override fun dispose() {
        retry?.cancel(false)
        liveRefresh?.cancel(false)
        socket?.abort()
        socket = null
    }

    companion object {
        fun getInstance(): DaemonClient = service()

        fun notify(html: String, type: NotificationType, project: Project? = null) {
            NotificationGroupManager.getInstance().getNotificationGroup("OpenProfiler")
                .createNotification(html, type)
                .notify(project)
        }

        private fun publish(f: (DaemonListener) -> Unit) {
            val app = ApplicationManager.getApplication()
            if (!app.isDisposed) f(app.messageBus.syncPublisher(DaemonListener.TOPIC))
        }
    }
}
