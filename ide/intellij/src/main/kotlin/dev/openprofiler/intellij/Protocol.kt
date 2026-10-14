// Plain data and helpers for the daemon's IDE bridge (docs/spec/query-api.md, /api/v1/ide).
package dev.openprofiler.intellij

import com.google.gson.JsonParser
import java.net.URI
import java.net.URLEncoder
import java.nio.file.Path

/** Where the daemon listens, from `~/.openprofiler/daemon.json` or the settings. */
data class Discovery(val url: String, val token: String) {
    fun ideUri(): URI =
        URI.create(url.trimEnd('/').replaceFirst(Regex("^http"), "ws") + "/api/v1/ide?token=" + URLEncoder.encode(token, Charsets.UTF_8))

    companion object {
        fun file(): Path = Path.of(System.getProperty("user.home"), ".openprofiler", "daemon.json")

        fun parse(json: String): Discovery? = runCatching {
            val o = JsonParser.parseString(json).asJsonObject
            Discovery(o["url"].asString, o["token"].asString)
        }.getOrNull()
    }
}

/** The native agent and the options to launch a JVM with, as returned by the daemon's `agent` request. */
data class AgentLaunch(val path: String, val options: String) {
    fun vmOption(infofile: String? = null): String = "-agentpath:$path=$options" + (infofile?.let { ",infofile=$it" } ?: "")
}

/** One line of `gutterData`. */
data class LineHeat(
    val `class`: String,
    val method: String,
    val line: Int,
    val self_percent: Double,
    val total_percent: Double,
)

/** Heat bucket 0..3 for a total CPU percentage; the same thresholds as the VS Code extension. */
fun heatLevel(totalPercent: Double): Int = when {
    totalPercent >= 25 -> 3
    totalPercent >= 10 -> 2
    totalPercent >= 3 -> 1
    else -> 0
}

/** JVM class names declared by a source file, from its package, the file name and the classes PSI found. */
fun classNames(packageName: String, fileName: String, declared: List<String>): List<String> {
    val base = fileName.substringBeforeLast('.')
    val fromFile = if (packageName.isEmpty()) base else "$packageName.$base"
    val names = linkedSetOf<String>()
    names += declared
    names += fromFile
    if (fileName.endsWith(".kt")) names += "${fromFile}Kt"
    return names.toList()
}
