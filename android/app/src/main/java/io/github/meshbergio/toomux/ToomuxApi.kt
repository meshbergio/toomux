package io.github.meshbergio.toomux

import org.json.JSONArray
import org.json.JSONObject
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URI
import java.net.URLEncoder
import java.nio.charset.StandardCharsets

class ApiException(val status: Int, message: String) : Exception(message)

class ToomuxApi(endpoint: String, private val token: String? = null) {
    val endpoint: String = normalizeEndpoint(endpoint)

    fun health(): String = request("GET", "/api/v1/health", null, authenticated = false)

    fun pair(code: String, deviceName: String): PairResult {
        val body = JSONObject().put("code", code).put("device_name", deviceName).toString()
        return SnapshotParser.pair(request("POST", "/api/v1/pair", body, authenticated = false))
    }

    fun snapshot(): Snapshot = SnapshotParser.parse(request("GET", "/api/v1/snapshot"))

    fun screen(sessionId: String, lines: Int = 120): ScreenResult =
        SnapshotParser.screen(
            request(
                "GET",
                "/api/v1/sessions/${segment(sessionId)}/screen?lines=${lines.coerceIn(10, 200)}",
            ),
        )

    fun prompt(sessionId: String, text: String) {
        request(
            "POST",
            "/api/v1/sessions/${segment(sessionId)}/prompt",
            JSONObject().put("text", text).toString(),
        )
    }

    fun keys(sessionId: String, keys: List<String>) {
        request(
            "POST",
            "/api/v1/sessions/${segment(sessionId)}/keys",
            JSONObject().put("keys", JSONArray(keys)).toString(),
        )
    }

    fun revokeSelf() {
        request("POST", "/api/v1/device/revoke", "{}")
    }

    fun tuiFrame(cols: Int, rows: Int, since: String? = null): TuiFrame {
        val suffix = buildString {
            append("?cols=")
            append(cols)
            append("&rows=")
            append(rows)
            if (!since.isNullOrBlank()) {
                append("&since=")
                append(segment(since))
            }
        }
        return SnapshotParser.tuiFrame(request("GET", "/api/v1/tui/frame$suffix"))
    }

    fun tuiKey(key: String) {
        tuiInput(JSONObject().put("kind", "key").put("key", key))
    }

    fun tuiText(text: String) {
        tuiInput(JSONObject().put("kind", "text").put("text", text))
    }

    fun tuiTap(x: Int, y: Int, right: Boolean = false) {
        tuiInput(
            JSONObject()
                .put("kind", "tap")
                .put("x", x)
                .put("y", y)
                .put("button", if (right) "right" else "left"),
        )
    }

    fun tuiScroll(x: Int, y: Int, delta: Int) {
        tuiInput(
            JSONObject()
                .put("kind", "scroll")
                .put("x", x)
                .put("y", y)
                .put("delta", delta.coerceIn(-1, 1)),
        )
    }

    fun closeTui() {
        request("POST", "/api/v1/tui/close", "{}")
    }

    fun memoryGraph(): MemoryGraph =
        SnapshotParser.graph(request("GET", "/api/v1/memory/graph", readTimeoutMs = 30_000))

    fun memoryPage(): String =
        JSONObject(request("GET", "/api/v1/memory/page", readTimeoutMs = 30_000)).getString("html")

    private fun tuiInput(body: JSONObject) {
        request("POST", "/api/v1/tui/input", body.toString())
    }

    private fun request(
        method: String,
        path: String,
        body: String? = null,
        authenticated: Boolean = true,
        readTimeoutMs: Int = 8_000,
    ): String {
        val connection = URI.create(endpoint + path).toURL().openConnection() as HttpURLConnection
        try {
            connection.requestMethod = method
            connection.connectTimeout = 3_000
            connection.readTimeout = readTimeoutMs
            connection.useCaches = false
            connection.setRequestProperty("Accept", "application/json")
            if (authenticated) {
                val credential = token ?: throw ApiException(401, "This device is not paired")
                connection.setRequestProperty("Authorization", "Bearer $credential")
            }
            if (body != null) {
                val bytes = body.toByteArray(StandardCharsets.UTF_8)
                connection.doOutput = true
                connection.setFixedLengthStreamingMode(bytes.size)
                connection.setRequestProperty("Content-Type", "application/json; charset=utf-8")
                connection.outputStream.use { it.write(bytes) }
            }
            val status = connection.responseCode
            val input = if (status in 200..299) connection.inputStream else connection.errorStream
            val text = input.readUtf8()
            if (status !in 200..299) {
                val message = try {
                    JSONObject(text).optString("error", "Request failed")
                } catch (_: Exception) {
                    "Request failed"
                }
                throw ApiException(status, message)
            }
            return text
        } finally {
            connection.disconnect()
        }
    }

    companion object {
        const val DEFAULT_ENDPOINT = "http://10.30.0.1:7462"

        fun normalizeEndpoint(raw: String): String {
            val clean = raw.trim().trimEnd('/')
            val uri = try {
                URI.create(clean)
            } catch (_: Exception) {
                throw IllegalArgumentException("Endpoint must be a URL")
            }
            if (uri.scheme != "http" || uri.userInfo != null || uri.rawQuery != null || uri.rawFragment != null) {
                throw IllegalArgumentException("Endpoint must be an http:// address")
            }
            val host = uri.host ?: throw IllegalArgumentException("Endpoint needs an IP address")
            val mesh = isByteTraverseIpv4(host)
            val loopback = host == "127.0.0.1" || host == "localhost"
            if (!mesh && !loopback) {
                throw IllegalArgumentException("Endpoint must be on ByteTraverse (10.30.x.x)")
            }
            if (uri.port !in 1..65535) {
                throw IllegalArgumentException("Endpoint needs an explicit port")
            }
            if (uri.path != null && uri.path != "" && uri.path != "/") {
                throw IllegalArgumentException("Endpoint must not include a path")
            }
            return clean
        }

        private fun isByteTraverseIpv4(host: String): Boolean {
            val parts = host.split('.')
            if (parts.size != 4) return false
            val octets = parts.map { part ->
                if (part.isEmpty() || (part.length > 1 && part.startsWith('0'))) return false
                part.toIntOrNull()?.takeIf { it in 0..255 } ?: return false
            }
            return octets[0] == 10 && octets[1] == 30
        }

        private fun segment(value: String): String =
            URLEncoder.encode(value, StandardCharsets.UTF_8.name()).replace("+", "%20")
    }
}

private fun InputStream?.readUtf8(): String {
    if (this == null) return ""
    return bufferedReader(StandardCharsets.UTF_8).use { it.readText() }
}
