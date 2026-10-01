package io.github.meshbergio.toomux

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class ToomuxApiTest {
    @Test
    fun bytetraverseAndLoopbackEndpointsAreAccepted() {
        assertEquals(
            "http://10.30.0.1:7462",
            ToomuxApi.normalizeEndpoint(" http://10.30.0.1:7462/ "),
        )
        assertEquals(
            "http://127.0.0.1:7462",
            ToomuxApi.normalizeEndpoint("http://127.0.0.1:7462"),
        )
    }

    @Test
    fun lanAndPublicEndpointsAreRejected() {
        assertThrows(IllegalArgumentException::class.java) {
            ToomuxApi.normalizeEndpoint("http://192.168.50.3:7462")
        }
        assertThrows(IllegalArgumentException::class.java) {
            ToomuxApi.normalizeEndpoint("https://10.30.0.1:7462")
        }
        assertThrows(IllegalArgumentException::class.java) {
            ToomuxApi.normalizeEndpoint("http://10.30.0.1:7462/api")
        }
        assertThrows(IllegalArgumentException::class.java) {
            ToomuxApi.normalizeEndpoint("http://10.30.evil.example:7462")
        }
        assertThrows(IllegalArgumentException::class.java) {
            ToomuxApi.normalizeEndpoint("http://10.30.0.1.evil:7462")
        }
        assertThrows(IllegalArgumentException::class.java) {
            ToomuxApi.normalizeEndpoint("http://10.30.0.01:7462")
        }
        assertThrows(IllegalArgumentException::class.java) {
            ToomuxApi.normalizeEndpoint("http://10.30.0.1")
        }
    }

    @Test
    fun sessionPriorityPutsHumanAttentionFirst() {
        fun session(state: String, waiting: String? = null) = RemoteSession(
            id = state,
            title = state,
            topic = null,
            place = "project",
            account = "default",
            state = state,
            detail = null,
            age = null,
            waitingFor = waiting,
            context = null,
            tokens = null,
            cost = null,
            model = null,
            pane = "%1@server",
        )
        val ordered = listOf(
            session("idle"),
            session("working"),
            session("needs you", "approval"),
            session("finished"),
        ).sortedBy { it.rank }
        assertEquals(listOf("needs you", "working", "finished", "idle"), ordered.map { it.state })
    }
}
