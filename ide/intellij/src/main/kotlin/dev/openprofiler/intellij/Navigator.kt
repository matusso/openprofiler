package dev.openprofiler.intellij

import com.intellij.ide.impl.ProjectUtil
import com.intellij.notification.NotificationType
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.application.ModalityState
import com.intellij.openapi.fileEditor.OpenFileDescriptor
import com.intellij.openapi.project.DumbService
import com.intellij.openapi.project.Project
import com.intellij.openapi.project.ProjectManager
import com.intellij.pom.Navigatable
import com.intellij.psi.JavaPsiFacade
import com.intellij.psi.PsiClass
import com.intellij.psi.search.GlobalSearchScope

/** Handles the daemon's `navigate {class, method?, line?}`: opens the source in the project that has the class. */
object Navigator {
    fun navigate(className: String, method: String?, line: Int?) {
        ApplicationManager.getApplication().invokeLater({ doNavigate(className, method, line) }, ModalityState.nonModal())
    }

    private fun doNavigate(className: String, method: String?, line: Int?) {
        val projects = ProjectManager.getInstance().openProjects.filter { !it.isDisposed && !DumbService.isDumb(it) }
        val (project, cls) = find(projects, className, GlobalSearchScope::projectScope)
            ?: find(projects, className, GlobalSearchScope::allScope)
            ?: run {
                DaemonClient.notify("Source for $className not found in the open projects", NotificationType.INFORMATION)
                return
            }
        val target = cls.navigationElement
        val file = target.containingFile?.virtualFile
        val navigatable: Navigatable? = when {
            file != null && line != null && line > 0 -> OpenFileDescriptor(project, file, line - 1, 0)
            method == "<init>" -> cls.constructors.firstOrNull()?.navigationElement as? Navigatable
            method != null -> cls.findMethodsByName(method, false).firstOrNull()?.navigationElement as? Navigatable
            else -> null
        } ?: target as? Navigatable
        navigatable?.navigate(true)
        ProjectUtil.focusProjectWindow(project, true)
    }

    /** Finds the top-level class (nested classes are in its file) in the first project that has it. */
    private fun find(projects: List<Project>, className: String, scope: (Project) -> GlobalSearchScope): Pair<Project, PsiClass>? {
        val top = className.substringBefore('$')
        for (p in projects) {
            JavaPsiFacade.getInstance(p).findClass(top, scope(p))?.let { return p to it }
        }
        return null
    }
}
