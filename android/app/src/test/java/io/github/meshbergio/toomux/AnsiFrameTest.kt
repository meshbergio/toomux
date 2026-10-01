package io.github.meshbergio.toomux

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class AnsiFrameTest {
    @Test
    fun trueColourBoldAndResetBecomeStyledRuns() {
        val screen = AnsiFrameParser().parse(
            "plain \u001b[1;38;2;125;211;252mcyan\u001b[0m end",
            40,
            2,
        )
        assertEquals("plain ", screen.lines[0][0].text)
        assertEquals("cyan", screen.lines[0][1].text)
        assertEquals(0xff7dd3fc.toInt(), screen.lines[0][1].fg)
        assertTrue(screen.lines[0][1].attrs and TerminalAttrs.BOLD != 0)
        assertEquals(0xffdbe2ec.toInt(), screen.lines[0][2].fg)
        assertEquals(0xff0f141c.toInt(), screen.lines[0][2].bg)
    }

    @Test
    fun xtermBackgroundAndWideGlyphsStayCellAccurate() {
        val screen = AnsiFrameParser().parse(
            "\u001b[48;5;236m界\u001b[0mb",
            10,
            1,
        )
        assertEquals("界", screen.lines[0][0].text)
        assertEquals(2, screen.lines[0][0].columns)
        assertEquals(0xff303030.toInt(), screen.lines[0][0].bg)
        assertEquals("b", screen.lines[0][1].text)
        assertEquals(2, AnsiFrameParser.cellWidth('界'.code))
    }

    @Test
    fun inverseSwapsTheRenderedForegroundAndBackground() {
        val screen = AnsiFrameParser(
            defaultFg = 0xff010203.toInt(),
            defaultBg = 0xff101112.toInt(),
        ).parse("\u001b[7mselected\u001b[27m", 20, 1)

        assertEquals(0xff101112.toInt(), screen.lines[0][0].fg)
        assertEquals(0xff010203.toInt(), screen.lines[0][0].bg)
    }
}
