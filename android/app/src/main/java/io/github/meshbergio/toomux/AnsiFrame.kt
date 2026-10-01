package io.github.meshbergio.toomux

data class TerminalScreen(
    val cols: Int,
    val rows: Int,
    val lines: List<List<TerminalRun>>,
)

data class TerminalRun(
    val start: Int,
    val columns: Int,
    val text: String,
    val fg: Int,
    val bg: Int,
    val attrs: Int,
)

object TerminalAttrs {
    const val BOLD = 1
    const val DIM = 1 shl 1
    const val ITALIC = 1 shl 2
    const val UNDERLINE = 1 shl 3
    const val STRIKE = 1 shl 4
}

class AnsiFrameParser(
    private val defaultFg: Int = rgb(0xDB, 0xE2, 0xEC),
    private val defaultBg: Int = rgb(0x0F, 0x14, 0x1C),
) {
    private data class Look(val fg: Int, val bg: Int, val attrs: Int)

    private data class State(
        var fg: Int,
        var bg: Int,
        var attrs: Int = 0,
        var inverse: Boolean = false,
    ) {
        fun look(): Look = if (inverse) Look(bg, fg, attrs) else Look(fg, bg, attrs)
    }

    fun parse(ansi: String, cols: Int, rows: Int): TerminalScreen {
        val safeCols = cols.coerceAtLeast(1)
        val safeRows = rows.coerceAtLeast(1)
        val lines = MutableList(safeRows) { mutableListOf<TerminalRun>() }
        val state = State(defaultFg, defaultBg)

        var row = 0
        var col = 0
        var i = 0
        var runStart = 0
        var runColumns = 0
        var runLook = state.look()
        val runText = StringBuilder()

        fun flush() {
            if (runText.isNotEmpty() && row in lines.indices) {
                lines[row] += TerminalRun(
                    start = runStart,
                    columns = runColumns,
                    text = runText.toString(),
                    fg = runLook.fg,
                    bg = runLook.bg,
                    attrs = runLook.attrs,
                )
            }
            runText.setLength(0)
            runColumns = 0
        }

        fun startRunIfNeeded() {
            val look = state.look()
            if (runText.isNotEmpty() && look != runLook) flush()
            if (runText.isEmpty()) {
                runStart = col
                runLook = look
            }
        }

        while (i < ansi.length && row < safeRows) {
            val ch = ansi[i]

            if (ch == '\u001b') {
                flush()
                if (i + 1 < ansi.length && ansi[i + 1] == '[') {
                    var end = i + 2
                    while (end < ansi.length && ansi[end] !in '@'..'~') end++
                    if (end >= ansi.length) break
                    if (ansi[end] == 'm') {
                        applySgr(state, ansi.substring(i + 2, end))
                    }
                    i = end + 1
                    continue
                }
                if (i + 1 < ansi.length && ansi[i + 1] == ']') {
                    var end = i + 2
                    while (end < ansi.length) {
                        if (ansi[end] == '\u0007') {
                            end++
                            break
                        }
                        if (
                            ansi[end] == '\u001b' &&
                            end + 1 < ansi.length &&
                            ansi[end + 1] == '\\'
                        ) {
                            end += 2
                            break
                        }
                        end++
                    }
                    i = end
                    continue
                }
                i += 2
                continue
            }

            when (ch) {
                '\n' -> {
                    flush()
                    row++
                    col = 0
                    i++
                    continue
                }
                '\r' -> {
                    col = 0
                    i++
                    continue
                }
                '\t' -> {
                    startRunIfNeeded()
                    val available = (safeCols - col).coerceAtLeast(0)
                    val spaces = (8 - (col % 8)).coerceAtMost(available)
                    if (spaces > 0) {
                        runText.append(" ".repeat(spaces))
                        runColumns += spaces
                        col += spaces
                    }
                    i++
                    continue
                }
            }

            if (ch.code < 0x20 || ch.code == 0x7f) {
                i++
                continue
            }

            val codePoint = Character.codePointAt(ansi, i)
            val glyph = String(Character.toChars(codePoint))
            val width = cellWidth(codePoint)
            if (width == 0) {
                if (runText.isNotEmpty()) runText.append(glyph)
                i += Character.charCount(codePoint)
                continue
            }
            if (col >= safeCols) {
                i += Character.charCount(codePoint)
                continue
            }

            startRunIfNeeded()
            runText.append(glyph)
            val used = width.coerceAtMost(safeCols - col)
            runColumns += used
            col += used
            i += Character.charCount(codePoint)
        }

        flush()
        return TerminalScreen(safeCols, safeRows, lines)
    }

    private fun applySgr(state: State, raw: String) {
        val params = if (raw.isBlank()) {
            listOf(0)
        } else {
            raw.split(';').map { it.toIntOrNull() ?: 0 }
        }

        var i = 0
        while (i < params.size) {
            when (val code = params[i]) {
                0 -> {
                    state.fg = defaultFg
                    state.bg = defaultBg
                    state.attrs = 0
                    state.inverse = false
                }
                1 -> state.attrs = state.attrs or TerminalAttrs.BOLD
                2 -> state.attrs = state.attrs or TerminalAttrs.DIM
                3 -> state.attrs = state.attrs or TerminalAttrs.ITALIC
                4 -> state.attrs = state.attrs or TerminalAttrs.UNDERLINE
                7 -> state.inverse = true
                9 -> state.attrs = state.attrs or TerminalAttrs.STRIKE
                22 -> state.attrs = state.attrs and
                    (TerminalAttrs.BOLD or TerminalAttrs.DIM).inv()
                23 -> state.attrs = state.attrs and TerminalAttrs.ITALIC.inv()
                24 -> state.attrs = state.attrs and TerminalAttrs.UNDERLINE.inv()
                27 -> state.inverse = false
                29 -> state.attrs = state.attrs and TerminalAttrs.STRIKE.inv()
                in 30..37 -> state.fg = indexed(code - 30)
                39 -> state.fg = defaultFg
                in 40..47 -> state.bg = indexed(code - 40)
                49 -> state.bg = defaultBg
                in 90..97 -> state.fg = indexed(code - 90 + 8)
                in 100..107 -> state.bg = indexed(code - 100 + 8)
                38, 48 -> {
                    val foreground = code == 38
                    when (params.getOrNull(i + 1)) {
                        5 -> {
                            val value = params.getOrNull(i + 2)
                            if (value != null && value in 0..255) {
                                if (foreground) state.fg = indexed(value) else state.bg = indexed(value)
                                i += 2
                            }
                        }
                        2 -> {
                            val red = params.getOrNull(i + 2)
                            val green = params.getOrNull(i + 3)
                            val blue = params.getOrNull(i + 4)
                            if (
                                red != null && green != null && blue != null &&
                                red in 0..255 && green in 0..255 && blue in 0..255
                            ) {
                                val value = rgb(red, green, blue)
                                if (foreground) state.fg = value else state.bg = value
                                i += 4
                            }
                        }
                    }
                }
            }
            i++
        }
    }

    companion object {
        fun cellWidth(codePoint: Int): Int {
            val type = Character.getType(codePoint)
            if (
                type == Character.NON_SPACING_MARK.toInt() ||
                type == Character.ENCLOSING_MARK.toInt() ||
                type == Character.COMBINING_SPACING_MARK.toInt()
            ) {
                return 0
            }
            return if (
                codePoint in 0x1100..0x115f ||
                codePoint in 0x2329..0x232a ||
                (codePoint in 0x2e80..0xa4cf && codePoint != 0x303f) ||
                codePoint in 0xac00..0xd7a3 ||
                codePoint in 0xf900..0xfaff ||
                codePoint in 0xfe10..0xfe19 ||
                codePoint in 0xfe30..0xfe6f ||
                codePoint in 0xff00..0xff60 ||
                codePoint in 0xffe0..0xffe6 ||
                codePoint in 0x1f300..0x1faff ||
                codePoint in 0x20000..0x3fffd
            ) {
                2
            } else {
                1
            }
        }

        private fun rgb(red: Int, green: Int, blue: Int): Int =
            (0xff shl 24) or (red shl 16) or (green shl 8) or blue

        private fun indexed(index: Int): Int {
            val basic = arrayOf(
                intArrayOf(0, 0, 0),
                intArrayOf(205, 49, 49),
                intArrayOf(13, 188, 121),
                intArrayOf(229, 229, 16),
                intArrayOf(36, 114, 200),
                intArrayOf(188, 63, 188),
                intArrayOf(17, 168, 205),
                intArrayOf(229, 229, 229),
                intArrayOf(102, 102, 102),
                intArrayOf(241, 76, 76),
                intArrayOf(35, 209, 139),
                intArrayOf(245, 245, 67),
                intArrayOf(59, 142, 234),
                intArrayOf(214, 112, 214),
                intArrayOf(41, 184, 219),
                intArrayOf(255, 255, 255),
            )
            if (index < 16) {
                val c = basic[index.coerceIn(0, 15)]
                return rgb(c[0], c[1], c[2])
            }
            if (index >= 232) {
                val value = 8 + (index.coerceAtMost(255) - 232) * 10
                return rgb(value, value, value)
            }
            val n = index - 16
            fun level(value: Int): Int = if (value == 0) 0 else 55 + 40 * value
            return rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
    }
}
