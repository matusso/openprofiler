package dev.openprofiler.intellij

import com.intellij.credentialStore.CredentialAttributes
import com.intellij.credentialStore.generateServiceName
import com.intellij.ide.passwordSafe.PasswordSafe
import com.intellij.openapi.components.BaseState
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.SimplePersistentStateComponent
import com.intellij.openapi.components.State
import com.intellij.openapi.components.Storage
import com.intellij.openapi.components.service
import com.intellij.openapi.options.BoundConfigurable
import com.intellij.openapi.ui.DialogPanel
import com.intellij.ui.dsl.builder.bindText
import com.intellij.ui.dsl.builder.columns
import com.intellij.ui.dsl.builder.panel
import java.nio.file.Files

@Service(Service.Level.APP)
@State(name = "OpenProfilerSettings", storages = [Storage("openprofiler.xml")])
class OpenProfilerSettings : SimplePersistentStateComponent<OpenProfilerSettings.Options>(Options()) {
    class Options : BaseState() {
        var daemonUrl by string("")
        var agentPath by string("")
        var minPercent by property(0.5f)
    }

    /** The daemon token lives in the password safe, not in openprofiler.xml. */
    var token: String
        get() = PasswordSafe.instance.getPassword(TOKEN) ?: ""
        set(value) = PasswordSafe.instance.setPassword(TOKEN, value.ifEmpty { null })

    /** The configured daemon, else `~/.openprofiler/daemon.json`. Reads the password safe and a file: call off the EDT. */
    fun discovery(): Discovery? {
        val url = state.daemonUrl.orEmpty()
        val token = token
        if (url.isNotEmpty() && token.isNotEmpty()) return Discovery(url, token)
        return runCatching { Discovery.parse(Files.readString(Discovery.file())) }.getOrNull()
    }

    companion object {
        private val TOKEN = CredentialAttributes(generateServiceName("OpenProfiler", "daemon token"))

        fun getInstance(): OpenProfilerSettings = service()
    }
}

class OpenProfilerConfigurable : BoundConfigurable("OpenProfiler") {
    private val settings = OpenProfilerSettings.getInstance()
    private var token = settings.token

    override fun createPanel(): DialogPanel = panel {
        val s = settings.state
        group("Daemon") {
            row("URL:") {
                textField().bindText({ s.daemonUrl.orEmpty() }, { s.daemonUrl = it })
                    .comment("Leave empty to read ~/.openprofiler/daemon.json, written by <code>openprofiler daemon</code>.")
            }
            row("Token:") {
                passwordField().bindText({ token }, { token = it })
            }
        }
        group("Agent") {
            row("Agent library:") {
                textField().bindText({ s.agentPath.orEmpty() }, { s.agentPath = it })
                    .comment("Path to libopenprofiler_agent.so / .dylib. Leave empty to use the agent the daemon uses.")
            }
        }
        group("CPU Heat") {
            row("Hide lines below:") {
                textField().columns(6).bindText({ s.minPercent.toString() }, { s.minPercent = it.toFloatOrNull() ?: 0.5f })
                label("% of total CPU")
            }
        }
    }

    override fun apply() {
        super.apply()
        settings.token = token
        DaemonClient.getInstance().reconnect()
    }
}
