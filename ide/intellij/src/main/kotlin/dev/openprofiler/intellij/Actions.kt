package dev.openprofiler.intellij

import com.intellij.notification.NotificationType
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.fileChooser.FileChooser
import com.intellij.openapi.fileChooser.FileChooserDescriptor
import com.intellij.openapi.ide.CopyPasteManager
import com.intellij.openapi.project.Project
import com.intellij.openapi.startup.ProjectActivity
import java.awt.datatransfer.StringSelection

class ConnectAction : AnAction() {
    override fun actionPerformed(e: AnActionEvent) = DaemonClient.getInstance().reconnect()
}

class RefreshHeatAction : AnAction() {
    override fun getActionUpdateThread() = ActionUpdateThread.BGT

    override fun update(e: AnActionEvent) {
        e.presentation.isEnabled = e.project != null && DaemonClient.getInstance().isConnected
    }

    override fun actionPerformed(e: AnActionEvent) {
        CpuHeat.getInstance(e.project ?: return).refreshAll()
    }
}

class OpenProfileAction : AnAction() {
    override fun getActionUpdateThread() = ActionUpdateThread.BGT

    override fun update(e: AnActionEvent) {
        e.presentation.isEnabled = DaemonClient.getInstance().isConnected
    }

    override fun actionPerformed(e: AnActionEvent) {
        val descriptor = FileChooserDescriptor(true, false, false, false, false, false)
            .withTitle("Open Profile in OpenProfiler")
            .withExtensionFilter("Profiles", "jfr", "hprof", "opsnap", "oplink", "gz")
        val file = FileChooser.chooseFile(descriptor, e.project, null) ?: return
        DaemonClient.getInstance().call("openFile", mapOf("path" to file.path)).whenComplete { _, err ->
            if (err != null) DaemonClient.notify("Could not open ${file.name}: ${err.cause?.message ?: err.message}", NotificationType.ERROR, e.project)
        }
    }
}

class CopyAgentOptionAction : AnAction() {
    override fun actionPerformed(e: AnActionEvent) {
        val agent = DaemonClient.getInstance().agent() ?: AgentLaunch("/path/to/libopenprofiler_agent.so", "tls")
        CopyPasteManager.getInstance().setContents(StringSelection(agent.vmOption()))
        DaemonClient.notify(
            "Copied the OpenProfiler -agentpath option. Add it to VM options, or use Run | Profile with OpenProfiler.",
            NotificationType.INFORMATION,
            e.project,
        )
    }
}

/** Connects to the daemon and starts the CPU heat for each opened project. */
class OpenProfilerStartup : ProjectActivity {
    override suspend fun execute(project: Project) {
        CpuHeat.getInstance(project)
        DaemonClient.getInstance().ensureConnected()
    }
}
