package dev.openprofiler.intellij

import com.google.gson.JsonObject
import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.application.ModalityState
import com.intellij.openapi.application.ReadAction
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.service
import com.intellij.openapi.editor.Editor
import com.intellij.openapi.editor.EditorCustomElementRenderer
import com.intellij.openapi.editor.Inlay
import com.intellij.openapi.editor.colors.EditorFontType
import com.intellij.openapi.editor.markup.GutterIconRenderer
import com.intellij.openapi.editor.markup.HighlighterLayer
import com.intellij.openapi.editor.markup.RangeHighlighter
import com.intellij.openapi.editor.markup.TextAttributes
import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.openapi.fileEditor.FileEditorManagerListener
import com.intellij.openapi.fileEditor.TextEditor
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Key
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.psi.PsiClassOwner
import com.intellij.psi.PsiManager
import com.intellij.ui.JBColor
import com.intellij.util.concurrency.AppExecutorUtil
import com.intellij.util.ui.ColorIcon
import com.intellij.util.ui.JBUI
import java.awt.Color
import java.awt.Graphics
import java.awt.Rectangle
import javax.swing.Icon

/** CPU heat in the editor: a tinted line background, a gutter swatch with a tooltip and the total percent. */
@Service(Service.Level.PROJECT)
class CpuHeat(private val project: Project) : Disposable {
    init {
        ApplicationManager.getApplication().messageBus.connect(this).subscribe(
            DaemonListener.TOPIC,
            object : DaemonListener {
                override fun connectionChanged(connected: Boolean) = refreshAll()
                override fun heatChanged() = refreshAll()
            },
        )
    }

    fun refreshAll() {
        ApplicationManager.getApplication().invokeLater({
            for (fe in FileEditorManager.getInstance(project).allEditors) {
                if (fe is TextEditor) refresh(fe.editor, fe.file)
            }
        }, project.disposed)
    }

    fun refresh(editor: Editor, file: VirtualFile) {
        if (file.extension !in SOURCE_EXTENSIONS) return
        val client = DaemonClient.getInstance()
        if (!client.isConnected) {
            apply(editor, emptyList())
            return
        }
        ReadAction.nonBlocking<List<String>> { classesOf(file) }
            .expireWith(this)
            .submit(AppExecutorUtil.getAppExecutorService())
            .onSuccess { classes ->
                client.call("gutterData", mapOf("classes" to classes)).handle { res, err ->
                    val lines = if (err != null) emptyList() else parse(res as? JsonObject)
                    ApplicationManager.getApplication().invokeLater({ apply(editor, lines) }, ModalityState.any(), project.disposed)
                }
            }
    }

    private fun classesOf(file: VirtualFile): List<String> {
        val psi = PsiManager.getInstance(project).findFile(file) as? PsiClassOwner
        val declared = psi?.classes?.mapNotNull { it.qualifiedName }.orEmpty()
        return classNames(psi?.packageName.orEmpty(), file.name, declared)
    }

    private fun parse(res: JsonObject?): List<LineHeat> {
        val min = OpenProfilerSettings.getInstance().state.minPercent
        return res?.getAsJsonArray("lines")?.map { GSON.fromJson(it, LineHeat::class.java) }
            ?.filter { it.total_percent >= min && it.line >= 1 }
            .orEmpty()
    }

    private fun apply(editor: Editor, lines: List<LineHeat>) {
        if (editor.isDisposed) return
        editor.getUserData(HIGHLIGHTERS)?.forEach { editor.markupModel.removeHighlighter(it) }
        editor.getUserData(INLAYS)?.forEach { it.dispose() }
        val doc = editor.document
        val highlighters = mutableListOf<RangeHighlighter>()
        val inlays = mutableListOf<Inlay<*>>()
        // Several frames (methods, lambdas) can share a line; show the hottest.
        for (l in lines.groupBy { it.line }.values.map { same -> same.maxBy { it.total_percent } }) {
            if (l.line > doc.lineCount) continue
            val color = HEAT[heatLevel(l.total_percent)]
            val h = editor.markupModel.addLineHighlighter(l.line - 1, HighlighterLayer.CARET_ROW - 1, TextAttributes().apply { backgroundColor = color })
            h.gutterIconRenderer = HeatIcon(color, "OpenProfiler CPU: ${l.method} line ${l.line}: total ${l.total_percent}% · self ${l.self_percent}%")
            highlighters += h
            editor.inlayModel.addAfterLineEndElement(doc.getLineEndOffset(l.line - 1), false, PercentRenderer("  %.1f%%".format(l.total_percent)))
                ?.let { inlays += it }
        }
        editor.putUserData(HIGHLIGHTERS, highlighters)
        editor.putUserData(INLAYS, inlays)
    }

    override fun dispose() {}

    private data class HeatIcon(val color: Color, val tip: String) : GutterIconRenderer() {
        override fun getIcon(): Icon = ColorIcon(JBUI.scale(8), JBColor(opaque(color), opaque(color)))
        override fun getTooltipText(): String = tip
        override fun isNavigateAction(): Boolean = false
    }

    private class PercentRenderer(private val text: String) : EditorCustomElementRenderer {
        override fun calcWidthInPixels(inlay: Inlay<*>): Int =
            inlay.editor.contentComponent.getFontMetrics(font(inlay.editor)).stringWidth(text)

        override fun paint(inlay: Inlay<*>, g: Graphics, target: Rectangle, textAttributes: TextAttributes) {
            val editor = inlay.editor
            g.font = font(editor)
            g.color = JBColor.GRAY
            val fm = g.fontMetrics
            g.drawString(text, target.x, target.y + (target.height - fm.height) / 2 + fm.ascent)
        }

        private fun font(editor: Editor) = editor.colorsScheme.getFont(EditorFontType.ITALIC)
    }

    companion object {
        private val GSON = com.google.gson.Gson()
        private val SOURCE_EXTENSIONS = setOf("java", "kt", "scala")
        private val HIGHLIGHTERS = Key.create<List<RangeHighlighter>>("openprofiler.heat.highlighters")
        private val INLAYS = Key.create<List<Inlay<*>>>("openprofiler.heat.inlays")

        // The four heat levels of the VS Code extension; the dark variants are a little stronger.
        private val HEAT = listOf(0.08 to 0.14, 0.16 to 0.24, 0.28 to 0.38, 0.42 to 0.52).map { (light, dark) ->
            JBColor(Color(232, 89, 12, (light * 255).toInt()), Color(232, 89, 12, (dark * 255).toInt()))
        }

        private fun opaque(c: Color) = Color(c.red, c.green, c.blue)

        fun getInstance(project: Project): CpuHeat = project.service()
    }
}

/** Shows heat as soon as a source file opens. */
class HeatFileListener(private val project: Project) : FileEditorManagerListener {
    override fun fileOpened(source: FileEditorManager, file: VirtualFile) {
        for (fe in source.getEditors(file)) {
            if (fe is TextEditor) CpuHeat.getInstance(project).refresh(fe.editor, file)
        }
    }
}
