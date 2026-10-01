package io.github.meshbergio.toomux

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Path
import android.graphics.Typeface
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.util.AttributeSet
import android.view.GestureDetector
import android.view.HapticFeedbackConstants
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import kotlin.math.floor
import kotlin.math.max

class TuiView @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null,
) : View(context, attrs) {
    private var softwareKeyboardEnabled = false
    interface Listener {
        fun onGeometry(cols: Int, rows: Int)
        fun onTap(col: Int, row: Int, right: Boolean)
        fun onScroll(col: Int, row: Int, delta: Int)
        fun onKey(key: String)
        fun onText(text: String)
        fun onCommandPalette()
    }

    var listener: Listener? = null

    private val parser = AnsiFrameParser()
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG or Paint.SUBPIXEL_TEXT_FLAG).apply {
        // Lenovo's Android build exposes a proportional face through the
        // static Typeface.MONOSPACE object even though the named "monospace"
        // family resolves to DroidSansMono. Resolve the family by name so the
        // terminal cell advance is genuinely fixed across ASCII glyphs.
        typeface = Typeface.create("monospace", Typeface.NORMAL)
        isSubpixelText = true
    }
    private val linePaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeCap = Paint.Cap.SQUARE
    }
    private val handler = Handler(Looper.getMainLooper())

    private var screen: TerminalScreen? = null
    private var cursor = TuiCursor(0, 0, false)
    private var cellWidth = 1f
    private var cellHeight = 1f
    private var baselineOffset = 1f
    private var gridCols = 120
    private var gridRows = 40
    private var fontSp = if (resources.configuration.smallestScreenWidthDp >= 600) 13.5f else 11f
    private var lastScrollAt = 0L

    private val scaleDetector = ScaleGestureDetector(
        context,
        object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
            override fun onScale(detector: ScaleGestureDetector): Boolean {
                fontSp = (fontSp * detector.scaleFactor).coerceIn(7.5f, 22f)
                configureGrid(width, height)
                invalidate()
                return true
            }
        },
    )

    private val gestureDetector = GestureDetector(
        context,
        object : GestureDetector.SimpleOnGestureListener() {
            override fun onDown(e: MotionEvent): Boolean {
                requestFocus()
                return true
            }

            override fun onSingleTapConfirmed(e: MotionEvent): Boolean {
                val (col, row) = cellAt(e.x, e.y)
                listener?.onTap(col, row, false)
                return true
            }

            override fun onDoubleTap(e: MotionEvent): Boolean {
                // Preserve Toomux's desktop mouse grammar. The host detects
                // two clicks within 450 ms and opens the selected session.
                // The printed "keys → …" footer remains the touch affordance
                // for summoning Android's soft keyboard.
                val (col, row) = cellAt(e.x, e.y)
                listener?.onTap(col, row, false)
                listener?.onTap(col, row, false)
                return true
            }

            override fun onLongPress(e: MotionEvent) {
                performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
                val (col, row) = cellAt(e.x, e.y)
                if (row <= 3 && col <= 20) {
                    listener?.onCommandPalette()
                } else {
                    listener?.onTap(col, row, true)
                }
            }

            override fun onScroll(
                e1: MotionEvent?,
                e2: MotionEvent,
                distanceX: Float,
                distanceY: Float,
            ): Boolean {
                if (kotlin.math.abs(distanceY) < cellHeight * 0.45f) return true
                val now = System.currentTimeMillis()
                if (now - lastScrollAt < 55L) return true
                lastScrollAt = now
                val (col, row) = cellAt(e2.x, e2.y)
                listener?.onScroll(col, row, if (distanceY < 0f) 1 else -1)
                return true
            }
        },
    )

    init {
        setBackgroundColor(DEFAULT_BG)
        isFocusable = true
        isFocusableInTouchMode = true
        importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_YES
    }

    fun applyFrame(frame: TuiFrame) {
        cursor = frame.cursor
        if (!frame.same && frame.ansi != null) {
            screen = parser.parse(frame.ansi, frame.cols, frame.rows)
        }
        invalidate()
    }

    fun currentGeometry(): Pair<Int, Int> = gridCols to gridRows

    fun containsText(needle: String): Boolean =
        screen?.lines?.any { line -> lineText(line).contains(needle) } == true

    fun isLiveFocused(): Boolean =
        containsText("session keys") || containsText("alt-s sessions")

    /**
     * The desktop footer remains visually unchanged, but on touch hardware its
     * printed Alt shortcuts are tappable. This keeps Toomux's own command
     * vocabulary as the UI instead of adding a second Android toolbar.
     */
    fun shortcutAt(col: Int, row: Int): String? {
        val value = screen ?: return null
        val line = value.lines.getOrNull(row - 1) ?: return null
        val text = lineText(line)
        val zeroCol = col - 1
        for ((label, key) in TOUCH_SHORTCUTS) {
            var from = 0
            while (true) {
                val start = text.indexOf(label, from)
                if (start < 0) break
                if (zeroCol in start until (start + label.length)) return key
                from = start + label.length
            }
        }
        return null
    }

    private fun lineText(line: List<TerminalRun>): String {
        if (line.isEmpty()) return ""
        val out = StringBuilder()
        for (run in line) {
            while (out.length < run.start) out.append(' ')
            out.append(run.text)
        }
        return out.toString()
    }

    fun showKeyboard() {
        softwareKeyboardEnabled = true
        requestFocus()
        val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        imm.restartInput(this)
        imm.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
    }

    fun hideKeyboard() {
        softwareKeyboardEnabled = false
        val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        imm.hideSoftInputFromWindow(windowToken, 0)
        imm.restartInput(this)
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        configureGrid(w, h)
    }

    private fun configureGrid(w: Int, h: Int) {
        if (w <= 0 || h <= 0) return
        val scaledDensity =
            resources.displayMetrics.density * resources.configuration.fontScale
        paint.textSize = fontSp * scaledDensity
        val fm = paint.fontMetrics
        // Keep the terminal cell on the font's actual floating-point metrics.
        // Rounding each cell up by even a fraction of a pixel compounds across
        // a 100+ column TUI and makes later borders/text visibly drift left.
        cellHeight = (fm.descent - fm.ascent).coerceAtLeast(1f)
        cellWidth = paint.measureText("M").coerceAtLeast(1f)
        baselineOffset = -fm.ascent

        val nextCols = floor(w / cellWidth).toInt().coerceIn(MIN_COLS, MAX_COLS)
        val nextRows = floor(h / cellHeight).toInt().coerceIn(MIN_ROWS, MAX_ROWS)
        if (nextCols != gridCols || nextRows != gridRows) {
            gridCols = nextCols
            gridRows = nextRows
            listener?.onGeometry(gridCols, gridRows)
        }
    }

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        canvas.drawColor(DEFAULT_BG)
        val value = screen ?: return
        val visibleRows = minOf(value.rows, floor(height / cellHeight).toInt())

        for (row in 0 until visibleRows) {
            val top = row * cellHeight
            val baseline = top + baselineOffset
            for (run in value.lines.getOrElse(row) { emptyList() }) {
                if (run.start >= value.cols) continue
                val left = run.start * cellWidth
                val right = (run.start + run.columns).coerceAtMost(value.cols) * cellWidth

                paint.style = Paint.Style.FILL
                paint.color = run.bg
                paint.alpha = 255
                paint.isFakeBoldText = false
                paint.isUnderlineText = false
                paint.isStrikeThruText = false
                paint.textSkewX = 0f
                canvas.drawRect(left, top, right, top + cellHeight, paint)

                paint.color = run.fg
                paint.alpha = if (run.attrs and TerminalAttrs.DIM != 0) 160 else 255
                paint.isFakeBoldText = run.attrs and TerminalAttrs.BOLD != 0
                paint.isUnderlineText = run.attrs and TerminalAttrs.UNDERLINE != 0
                paint.isStrikeThruText = run.attrs and TerminalAttrs.STRIKE != 0
                paint.textSkewX = if (run.attrs and TerminalAttrs.ITALIC != 0) -0.18f else 0f
                drawRunText(canvas, run, top, baseline, value.cols)
            }
        }

        paint.alpha = 255
        paint.isFakeBoldText = false
        paint.isUnderlineText = false
        paint.isStrikeThruText = false
        paint.textSkewX = 0f

        if (cursor.visible && cursor.x in 0 until value.cols && cursor.y in 0 until value.rows) {
            val left = cursor.x * cellWidth
            val top = cursor.y * cellHeight
            paint.color = DEFAULT_FG
            paint.alpha = 210
            canvas.drawRect(
                left,
                top,
                left + max(2f, cellWidth * 0.10f),
                top + cellHeight,
                paint,
            )
            paint.alpha = 255
        }
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        scaleDetector.onTouchEvent(event)
        if (!scaleDetector.isInProgress) {
            gestureDetector.onTouchEvent(event)
        }
        return true
    }

    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        if (event.action == MotionEvent.ACTION_SCROLL) {
            val amount = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
            if (amount != 0f) {
                val (col, row) = cellAt(event.x, event.y)
                listener?.onScroll(col, row, if (amount > 0f) 1 else -1)
                return true
            }
        }
        return super.onGenericMotionEvent(event)
    }

    private fun cellAt(x: Float, y: Float): Pair<Int, Int> =
        ((floor(x / cellWidth).toInt() + 1).coerceIn(1, gridCols.coerceAtLeast(1))) to
            ((floor(y / cellHeight).toInt() + 1).coerceIn(1, gridRows.coerceAtLeast(1)))

    /**
     * Android may fall back to another font for box-drawing and symbol glyphs.
     * Drawing an entire run lets those fallback advances accumulate, while a
     * terminal requires every glyph to occupy an exact cell. Paint each
     * grapheme cluster into its explicit terminal slot so later columns can
     * never drift. The common Toomux box characters are drawn geometrically
     * to keep vertical/horizontal joins pixel-perfect across Android fonts.
     */
    private fun drawRunText(
        canvas: Canvas,
        run: TerminalRun,
        top: Float,
        baseline: Float,
        maxCols: Int,
    ) {
        var terminalCol = run.start
        var offset = 0
        while (offset < run.text.length && terminalCol < maxCols) {
            val codePoint = Character.codePointAt(run.text, offset)
            val width = AnsiFrameParser.cellWidth(codePoint)
            if (width == 0) {
                offset += Character.charCount(codePoint)
                continue
            }

            val cluster = StringBuilder().appendCodePoint(codePoint)
            offset += Character.charCount(codePoint)
            while (offset < run.text.length) {
                val next = Character.codePointAt(run.text, offset)
                if (AnsiFrameParser.cellWidth(next) != 0) break
                cluster.appendCodePoint(next)
                offset += Character.charCount(next)
            }

            val used = width.coerceAtMost(maxCols - terminalCol)
            val slotLeft = terminalCol * cellWidth
            val slotWidth = used * cellWidth
            if (!drawBoxGlyph(canvas, codePoint, slotLeft, top, slotWidth, cellHeight, paint.color)) {
                val text = cluster.toString()
                if (text != " ") {
                    val advance = paint.measureText(text)
                    // ASCII belongs to the actual monospace face and should
                    // start exactly on the terminal cell edge. Centering every
                    // character made normal words look artificially tracked
                    // out. Only fallback/non-ASCII glyphs are centred.
                    val x = if (codePoint in 0x20..0x7e && used == 1) {
                        slotLeft
                    } else {
                        slotLeft + ((slotWidth - advance) * 0.5f)
                    }
                    canvas.drawText(text, x, baseline, paint)
                }
            }
            terminalCol += used
        }
    }

    private fun drawBoxGlyph(
        canvas: Canvas,
        codePoint: Int,
        left: Float,
        top: Float,
        width: Float,
        height: Float,
        color: Int,
    ): Boolean {
        linePaint.color = color
        linePaint.alpha = paint.alpha
        linePaint.strokeWidth = max(1f, resources.displayMetrics.density * 0.72f)
        if (codePoint in ROUNDED_BOXES) {
            drawRoundedBoxCorner(canvas, codePoint, left, top, width, height)
            return true
        }
        val mask = when (codePoint) {
            0x2500 -> BOX_LEFT or BOX_RIGHT
            0x2502 -> BOX_UP or BOX_DOWN
            0x250C -> BOX_RIGHT or BOX_DOWN
            0x2510 -> BOX_LEFT or BOX_DOWN
            0x2514 -> BOX_RIGHT or BOX_UP
            0x2518 -> BOX_LEFT or BOX_UP
            0x251C -> BOX_RIGHT or BOX_UP or BOX_DOWN
            0x2524 -> BOX_LEFT or BOX_UP or BOX_DOWN
            0x252C -> BOX_LEFT or BOX_RIGHT or BOX_DOWN
            0x2534 -> BOX_LEFT or BOX_RIGHT or BOX_UP
            0x253C -> BOX_LEFT or BOX_RIGHT or BOX_UP or BOX_DOWN
            else -> return false
        }
        val cx = left + width * 0.5f
        val cy = top + height * 0.5f
        if (mask and BOX_LEFT != 0) canvas.drawLine(left, cy, cx, cy, linePaint)
        if (mask and BOX_RIGHT != 0) canvas.drawLine(cx, cy, left + width, cy, linePaint)
        if (mask and BOX_UP != 0) canvas.drawLine(cx, top, cx, cy, linePaint)
        if (mask and BOX_DOWN != 0) canvas.drawLine(cx, cy, cx, top + height, linePaint)
        return true
    }

    private fun drawRoundedBoxCorner(
        canvas: Canvas,
        codePoint: Int,
        left: Float,
        top: Float,
        width: Float,
        height: Float,
    ) {
        val cx = left + width * 0.5f
        val cy = top + height * 0.5f
        val radius = minOf(width, height) * 0.28f
        val path = Path()
        when (codePoint) {
            0x256D -> {
                path.moveTo(left + width, cy)
                path.lineTo(cx + radius, cy)
                path.quadTo(cx, cy, cx, cy + radius)
                path.lineTo(cx, top + height)
            }
            0x256E -> {
                path.moveTo(left, cy)
                path.lineTo(cx - radius, cy)
                path.quadTo(cx, cy, cx, cy + radius)
                path.lineTo(cx, top + height)
            }
            0x2570 -> {
                path.moveTo(left + width, cy)
                path.lineTo(cx + radius, cy)
                path.quadTo(cx, cy, cx, cy - radius)
                path.lineTo(cx, top)
            }
            0x256F -> {
                path.moveTo(left, cy)
                path.lineTo(cx - radius, cy)
                path.quadTo(cx, cy, cx, cy - radius)
                path.lineTo(cx, top)
            }
        }
        canvas.drawPath(path, linePaint)
    }

    override fun onCheckIsTextEditor(): Boolean = softwareKeyboardEnabled

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        if (!softwareKeyboardEnabled) return null
        outAttrs.inputType =
            InputType.TYPE_CLASS_TEXT or
            InputType.TYPE_TEXT_FLAG_MULTI_LINE or
            InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions =
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_ACTION_NONE

        return object : BaseInputConnection(this, false) {
            override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
                if (!text.isNullOrEmpty()) listener?.onText(text.toString())
                return true
            }

            override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
                return true
            }

            override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
                repeat(beforeLength.coerceIn(1, 16)) { listener?.onKey("BSpace") }
                return true
            }

            override fun sendKeyEvent(event: KeyEvent): Boolean {
                dispatchKeyEvent(event)
                return true
            }

            override fun performEditorAction(actionCode: Int): Boolean {
                listener?.onKey("Enter")
                return true
            }
        }
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        val named = when (keyCode) {
            KeyEvent.KEYCODE_ENTER,
            KeyEvent.KEYCODE_NUMPAD_ENTER -> "Enter"
            KeyEvent.KEYCODE_ESCAPE -> {
                if (isLiveFocused() && !event.isShiftPressed) {
                    listener?.onKey("M-s")
                    return true
                }
                "Escape"
            }
            KeyEvent.KEYCODE_TAB -> if (event.isShiftPressed) "BTab" else "Tab"
            KeyEvent.KEYCODE_DEL -> "BSpace"
            KeyEvent.KEYCODE_DPAD_UP -> "Up"
            KeyEvent.KEYCODE_DPAD_DOWN -> "Down"
            KeyEvent.KEYCODE_DPAD_LEFT -> "Left"
            KeyEvent.KEYCODE_DPAD_RIGHT -> "Right"
            KeyEvent.KEYCODE_MOVE_HOME -> "Home"
            KeyEvent.KEYCODE_MOVE_END -> "End"
            KeyEvent.KEYCODE_PAGE_UP -> "PPage"
            KeyEvent.KEYCODE_PAGE_DOWN -> "NPage"
            KeyEvent.KEYCODE_INSERT -> "IC"
            KeyEvent.KEYCODE_FORWARD_DEL -> "DC"
            in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 ->
                "F${keyCode - KeyEvent.KEYCODE_F1 + 1}"
            else -> null
        }

        if (named != null) {
            listener?.onKey(withModifiers(named, event, named == "BTab"))
            return true
        }

        val base = keyBase(event)
        if (base != null && (event.isCtrlPressed || event.isAltPressed)) {
            listener?.onKey(withModifiers(base.lowercase(), event, false))
            return true
        }

        val unicode = event.unicodeChar
        if (unicode != 0 && unicode and KeyCharacterMap.COMBINING_ACCENT == 0) {
            listener?.onText(String(Character.toChars(unicode)))
            return true
        }
        return super.onKeyDown(keyCode, event)
    }

    private fun withModifiers(base: String, event: KeyEvent, suppressShift: Boolean): String {
        val out = StringBuilder()
        if (event.isCtrlPressed) out.append("C-")
        if (event.isAltPressed) out.append("M-")
        if (event.isShiftPressed && !suppressShift) out.append("S-")
        out.append(base)
        return out.toString()
    }

    private fun keyBase(event: KeyEvent): String? = when (event.keyCode) {
        in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z ->
            ('a'.code + event.keyCode - KeyEvent.KEYCODE_A).toChar().toString()
        in KeyEvent.KEYCODE_0..KeyEvent.KEYCODE_9 ->
            ('0'.code + event.keyCode - KeyEvent.KEYCODE_0).toChar().toString()
        KeyEvent.KEYCODE_SPACE -> "Space"
        else -> event.unicodeChar
            .takeIf { it > 0 }
            ?.let { String(Character.toChars(it)).takeIf { value -> value.length <= 2 } }
    }

    companion object {
        private const val MIN_COLS = 48
        private const val MAX_COLS = 240
        private const val MIN_ROWS = 18
        private const val MAX_ROWS = 96
        private const val DEFAULT_BG = -0xF0EBE4
        private const val DEFAULT_FG = -0x241D14
        private const val BOX_LEFT = 1
        private const val BOX_RIGHT = 1 shl 1
        private const val BOX_UP = 1 shl 2
        private const val BOX_DOWN = 1 shl 3
        private val ROUNDED_BOXES = setOf(0x256D, 0x256E, 0x2570, 0x256F)
        private val TOUCH_SHORTCUTS = listOf(
            "alt-s sessions" to "M-s",
            "alt-u usage" to "M-u",
            "alt-m memory" to "M-m",
            "alt-b list" to "M-b",
            "alt-j go there" to "M-j",
            "enter focus" to "Enter",
            "enter open" to "Enter",
            "^a move" to "C-a",
            "/ find" to "__find__",
            "o browser" to "__memory_browser__",
            "esc close" to "Escape",
            "esc back" to "Escape",
            "esc sessions" to "M-s",
            "? keys" to "?",
            "? hide legend" to "?",
            "keys →" to "__keyboard__",
        )
    }
}
