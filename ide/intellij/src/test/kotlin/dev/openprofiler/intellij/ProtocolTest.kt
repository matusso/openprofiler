package dev.openprofiler.intellij

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ProtocolTest {
    @Test
    fun discovery() {
        val d = Discovery.parse("""{"url": "http://127.0.0.1:7878", "token": "a b+c", "pid": 1}""")!!
        assertEquals("ws://127.0.0.1:7878/api/v1/ide?token=a+b%2Bc", d.ideUri().toString())
        assertNull(Discovery.parse("""{"url": "http://127.0.0.1:7878"}"""))
        assertNull(Discovery.parse("not json"))
    }

    @Test
    fun agentOption() {
        val a = AgentLaunch("/opt/op/libopenprofiler_agent.so", "tls")
        assertEquals("-agentpath:/opt/op/libopenprofiler_agent.so=tls", a.vmOption())
        assertEquals("-agentpath:/opt/op/libopenprofiler_agent.so=tls,infofile=/tmp/x.json", a.vmOption("/tmp/x.json"))
    }

    @Test
    fun heatLevels() {
        assertEquals(listOf(0, 1, 2, 3), listOf(0.5, 3.0, 10.0, 25.0).map(::heatLevel))
    }

    @Test
    fun classNamesOfFiles() {
        assertEquals(listOf("a.b.Foo", "a.b.Foo\$Inner"), classNames("a.b", "Foo.java", listOf("a.b.Foo", "a.b.Foo\$Inner")).take(2))
        assertEquals(listOf("a.Util", "a.UtilKt"), classNames("a", "Util.kt", emptyList()))
        assertEquals(listOf("Main"), classNames("", "Main.java", emptyList()))
    }
}
