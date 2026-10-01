package io.github.meshbergio.toomux

import org.json.JSONArray
import org.json.JSONObject

data class Meter(val used: Double, val resetsMs: Long, val limited: Boolean)

data class AccountUsage(
    val name: String,
    val fiveHour: Meter?,
    val week: Meter?,
    val problem: String?,
    val atMs: Long,
)

data class RemoteSession(
    val id: String,
    val title: String,
    val topic: String?,
    val place: String,
    val account: String,
    val state: String,
    val detail: String?,
    val age: String?,
    val waitingFor: String?,
    val context: Double?,
    val tokens: Long?,
    val cost: Double?,
    val model: String?,
    val pane: String?,
) {
    val rank: Int
        get() = when {
            waitingFor != null || state.equals("needs you", ignoreCase = true) -> 0
            state.contains("handover", ignoreCase = true) -> 1
            state.equals("working", ignoreCase = true) ||
                state.contains("tasks running", ignoreCase = true) -> 2
            state.equals("finished", ignoreCase = true) -> 3
            state.equals("idle", ignoreCase = true) -> 4
            else -> 5
        }

    val isInteractive: Boolean get() = pane != null
}

data class Snapshot(
    val host: String,
    val nowMs: Long,
    val accounts: List<AccountUsage>,
    val sessions: List<RemoteSession>,
)

data class PairResult(val deviceId: String, val token: String)
data class ScreenResult(val sessionId: String, val title: String, val screen: String)

data class TuiFrame(
    val same: Boolean,
    val sha256: String,
    val cols: Int,
    val rows: Int,
    val ansi: String?,
    val cursor: TuiCursor,
)

data class TuiCursor(
    val x: Int,
    val y: Int,
    val visible: Boolean,
)

data class MemoryNode(
    val id: String,
    val kind: String,
    val label: String,
    val project: String?,
    val at: Long,
    val last: Long,
    val size: Long,
    val text: String,
    val path: String?,
    val entry: String?,
)

data class MemoryEdge(
    val from: Int,
    val to: Int,
    val kind: String,
    val count: Int,
)

data class MemoryGraph(
    val made: Long,
    val nodes: List<MemoryNode>,
    val edges: List<MemoryEdge>,
)

object SnapshotParser {
    fun parse(raw: String): Snapshot {
        val root = JSONObject(raw)
        return Snapshot(
            host = root.optString("host", "this machine"),
            nowMs = root.optLong("now_ms", 0L),
            accounts = root.optJSONArray("accounts").objects().map(::account),
            sessions = root.optJSONArray("sessions").objects().map(::session),
        )
    }

    fun pair(raw: String): PairResult {
        val root = JSONObject(raw)
        return PairResult(root.getString("device_id"), root.getString("token"))
    }

    fun screen(raw: String): ScreenResult {
        val root = JSONObject(raw)
        return ScreenResult(
            root.getString("session_id"),
            root.optString("title"),
            root.optString("screen"),
        )
    }

    fun tuiFrame(raw: String): TuiFrame {
        val root = JSONObject(raw)
        return TuiFrame(
            same = root.optBoolean("same", false),
            sha256 = root.optString("sha256"),
            cols = root.optInt("cols"),
            rows = root.optInt("rows"),
            ansi = root.nullableString("ansi"),
            cursor = root.optJSONObject("cursor")?.let {
                TuiCursor(
                    x = it.optInt("x"),
                    y = it.optInt("y"),
                    visible = it.optBoolean("visible"),
                )
            } ?: TuiCursor(0, 0, false),
        )
    }

    fun graph(raw: String): MemoryGraph {
        val root = JSONObject(raw)
        val nodes = root.optJSONArray("nodes").objects().map { v ->
            MemoryNode(
                id = v.getString("id"),
                kind = v.optString("kind"),
                label = v.optString("label"),
                project = v.nullableString("project"),
                at = v.optLong("at"),
                last = v.optLong("last"),
                size = v.optLong("size"),
                text = v.optString("text"),
                path = v.nullableString("path"),
                entry = v.nullableString("entry"),
            )
        }
        val edges = root.optJSONArray("edges").objects().map { v ->
            MemoryEdge(
                from = v.optInt("s"),
                to = v.optInt("t"),
                kind = v.optString("kind"),
                count = v.optInt("n", 1),
            )
        }
        return MemoryGraph(root.optLong("made"), nodes, edges)
    }

    private fun account(v: JSONObject) = AccountUsage(
        name = v.optString("name", "account"),
        fiveHour = v.optJSONObject("five_hour")?.let(::meter),
        week = v.optJSONObject("week")?.let(::meter),
        problem = v.nullableString("problem"),
        atMs = v.optLong("at_ms", 0L),
    )

    private fun meter(v: JSONObject) = Meter(
        used = v.optDouble("used", 0.0),
        resetsMs = v.optLong("resets_ms", 0L),
        limited = v.optBoolean("limited", false),
    )

    private fun session(v: JSONObject) = RemoteSession(
        id = v.getString("id"),
        title = v.optString("title", "session"),
        topic = v.nullableString("topic"),
        place = v.optString("place"),
        account = v.optString("account", "unknown"),
        state = v.optString("state", "unknown"),
        detail = v.nullableString("detail"),
        age = v.nullableString("age"),
        waitingFor = v.nullableString("waiting_for"),
        context = v.nullableDouble("context"),
        tokens = v.nullableLong("tokens"),
        cost = v.nullableDouble("cost"),
        model = v.nullableString("model"),
        pane = v.nullableString("pane"),
    )

    private fun JSONArray?.objects(): List<JSONObject> {
        if (this == null) return emptyList()
        return (0 until length()).mapNotNull { optJSONObject(it) }
    }

    private fun JSONObject.nullableString(name: String): String? =
        if (isNull(name)) null else optString(name).takeIf { it.isNotBlank() }

    private fun JSONObject.nullableDouble(name: String): Double? =
        if (isNull(name) || !has(name)) null else optDouble(name).takeUnless { it.isNaN() }

    private fun JSONObject.nullableLong(name: String): Long? =
        if (isNull(name) || !has(name)) null else optLong(name)
}
