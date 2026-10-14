package dev.openprofiler.intellij

import com.intellij.execution.CommonJavaRunConfigurationParameters
import com.intellij.execution.ExecutionException
import com.intellij.execution.Executor
import com.intellij.execution.RunConfigurationExtension
import com.intellij.execution.configurations.JavaParameters
import com.intellij.execution.configurations.RunConfigurationBase
import com.intellij.execution.configurations.RunProfile
import com.intellij.execution.configurations.RunnerSettings
import com.intellij.execution.impl.DefaultJavaProgramRunner
import com.intellij.execution.process.ProcessEvent
import com.intellij.execution.process.ProcessHandler
import com.intellij.execution.process.ProcessListener
import com.intellij.icons.AllIcons
import com.intellij.notification.NotificationType
import com.intellij.openapi.util.IconLoader
import com.intellij.openapi.util.Key
import com.intellij.openapi.wm.ToolWindowId
import java.nio.file.Files
import java.nio.file.Path
import javax.swing.Icon

/** "Profile with OpenProfiler": runs a Java configuration with the agent and connects the daemon to it. */
class ProfileExecutor : Executor() {
    override fun getToolWindowId(): String = ToolWindowId.RUN
    override fun getToolWindowIcon(): Icon = AllIcons.Toolwindows.ToolWindowRun
    override fun getIcon(): Icon = ICON
    override fun getDisabledIcon(): Icon = IconLoader.getDisabledIcon(ICON)
    override fun getDescription(): String = "Run the selected configuration with the OpenProfiler agent"
    override fun getActionName(): String = "Profile with OpenProfiler"
    override fun getId(): String = ID
    override fun getStartActionText(): String = "Profile with OpenProfiler"
    override fun getStartActionText(configurationName: String): String =
        if (configurationName.isEmpty()) startActionText else "Profile '$configurationName' with OpenProfiler"
    override fun getContextActionId(): String = "OpenProfiler.ProfileContext"
    override fun getHelpId(): String? = null

    companion object {
        const val ID = "OpenProfiler"
        val ICON: Icon = IconLoader.getIcon("/icons/openprofiler.svg", ProfileExecutor::class.java)
    }
}

/** Runs Java configurations (application, JUnit, Spring Boot, ...) for [ProfileExecutor] like a normal run. */
class ProfileRunner : DefaultJavaProgramRunner() {
    override fun getRunnerId(): String = "OpenProfilerRunner"

    override fun canRun(executorId: String, profile: RunProfile): Boolean =
        executorId == ProfileExecutor.ID && profile is CommonJavaRunConfigurationParameters
}

/** Adds `-agentpath` for [ProfileExecutor] runs and asks the daemon to connect once the process started. */
class AgentRunConfigurationExtension : RunConfigurationExtension() {
    override fun isApplicableFor(configuration: RunConfigurationBase<*>): Boolean = true

    override fun <T : RunConfigurationBase<*>> updateJavaParameters(configuration: T, params: JavaParameters, runnerSettings: RunnerSettings?) {}

    override fun <T : RunConfigurationBase<*>> updateJavaParameters(
        configuration: T,
        params: JavaParameters,
        runnerSettings: RunnerSettings?,
        executor: Executor,
    ) {
        if (executor.id != ProfileExecutor.ID) return
        val client = DaemonClient.getInstance()
        val agent = client.agent() ?: throw ExecutionException(
            if (client.isConnected) "The OpenProfiler daemon has no agent library. Set its path in Settings | Tools | OpenProfiler."
            else "Not connected to the OpenProfiler daemon. Start `openprofiler daemon`, or set the agent path in Settings | Tools | OpenProfiler.",
        )
        val infofile = Files.createTempFile("openprofiler-intellij-", ".json")
        Files.delete(infofile) // the agent publishes it once it listens
        params.vmParametersList.add(agent.vmOption(infofile.toString()))
        configuration.putUserData(INFOFILE, infofile)
    }

    override fun attachToProcess(configuration: RunConfigurationBase<*>, handler: ProcessHandler, runnerSettings: RunnerSettings?) {
        val infofile = configuration.getUserData(INFOFILE) ?: return
        configuration.putUserData(INFOFILE, null)
        val client = DaemonClient.getInstance()
        if (!client.isConnected) {
            DaemonClient.notify(
                "Started ${configuration.name} with the agent, but the daemon is not connected. Attach to it from OpenProfiler.",
                NotificationType.WARNING,
                configuration.project,
            )
            return
        }
        val attached = client.attachLaunched(configuration.project, infofile.toString(), configuration.name)
        handler.addProcessListener(object : ProcessListener {
            override fun processTerminated(event: ProcessEvent) {
                attached.thenRun { client.profiledRunEnded() }
                Files.deleteIfExists(infofile)
            }
        })
    }

    companion object {
        private val INFOFILE = Key.create<Path>("openprofiler.infofile")
    }
}

