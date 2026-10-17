package matrix

import dev.openprofiler.api.OpenProfiler
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withContext

/** Coroutines started by a request: async and launch on Default, withContext(IO), a delay. */
object Coroutines {
    fun work(what: String): String {
        OpenProfiler.probeEvent("work", what, 1000)
        return what
    }

    @JvmStatic
    fun handle(id: String): String = runBlocking {
        val owner = async(Dispatchers.Default) { work("async") }
        withContext(Dispatchers.IO) {
            work("io")
            delay(5)
            work("after delay")
        }
        launch(Dispatchers.Default) { work("launch") }.join()
        owner.await() + id
    }
}
