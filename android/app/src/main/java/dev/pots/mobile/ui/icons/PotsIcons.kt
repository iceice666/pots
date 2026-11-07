package dev.pots.mobile.ui.icons

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.addPathNodes
import androidx.compose.ui.unit.dp

/**
 * 設計稿裡的線條圖示，SVG path 原樣搬過來（24×24 viewBox）。
 *
 * 筆畫用黑色畫，`Icon(tint = …)` 會整個換色，所以呼叫端照常給顏色即可。
 * `<circle>`、`<rect>` 先換成等價的 path。
 */
object PotsIcons {
    val Server by lazy { stroke("Server", rect(3f, 4f, 18f, 7f, 2f), rect(3f, 13f, 18f, 7f, 2f), "M7 7.5h.01M7 16.5h.01") }
    val ChevronDown by lazy { stroke("ChevronDown", "m6 9 6 6 6-6") }
    val ChevronUp by lazy { stroke("ChevronUp", "m6 15 6-6 6 6") }
    val ChevronLeft by lazy { stroke("ChevronLeft", "m15 6-6 6 6 6") }
    val ChevronRight by lazy { stroke("ChevronRight", "m9 6 6 6-6 6") }
    val Search by lazy { stroke("Search", circle(11f, 11f, 7f), "m20 20-3.5-3.5") }
    val Plus by lazy { stroke("Plus", "M12 5v14M5 12h14") }
    val Warning by lazy {
        stroke(
            "Warning",
            "M12 9v4M12 17h.01",
            "M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z",
        )
    }
    val ArrowRight by lazy { stroke("ArrowRight", "M5 12h14M13 6l6 6-6 6") }
    val ArrowUp by lazy { stroke("ArrowUp", "M12 19V5M5 12l7-7 7 7") }
    val Hash by lazy { stroke("Hash", "M5 9h14M5 15h14M10 3 8 21M16 3l-2 18") }
    val Inbox by lazy {
        stroke(
            "Inbox",
            "M22 12h-6l-2 3h-4l-2-3H2",
            "M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z",
        )
    }
    val Compass by lazy { stroke("Compass", circle(12f, 12f, 9f), "m15.5 8.5-2 5-5 2 2-5z") }
    val Settings by lazy {
        stroke(
            "Settings",
            circle(12f, 12f, 3f),
            "M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M4.9 19.1 7 17M17 7l2.1-2.1",
        )
    }
    val Moon by lazy { stroke("Moon", "M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z") }
    val Clock by lazy { stroke("Clock", circle(12f, 12f, 9f), "M12 7v5l3 2") }
    val GitBranch by lazy { stroke("GitBranch", circle(6f, 6f, 2.5f), circle(18f, 18f, 2.5f), "M6 8.5V21M18 15.5V9a3 3 0 0 0-3-3h-4") }
    val Spinner by lazy { stroke("Spinner", "M12 3a9 9 0 1 0 9 9", width = 2.4f) }
    val Check by lazy { stroke("Check", "m5 12 5 5 9-10", width = 2.4f) }
    val Close by lazy { stroke("Close", "M6 6l12 12M18 6 6 18") }
    val Shield by lazy { stroke("Shield", "M12 3 4 6v6c0 5 3.5 8 8 9 4.5-1 8-4 8-9V6z") }
    val Monitor by lazy { stroke("Monitor", rect(3f, 4f, 18f, 12f, 2f), "M8 20h8M12 16v4") }
    val Panel by lazy { stroke("Panel", rect(3f, 3f, 18f, 18f, 2f), "M15 3v18") }
    val More by lazy { fill("More", circle(5f, 12f, 1.8f), circle(12f, 12f, 1.8f), circle(19f, 12f, 1.8f)) }
}

private fun circle(cx: Float, cy: Float, r: Float): String =
    "M${cx - r} ${cy}a$r $r 0 1 0 ${2 * r} 0a$r $r 0 1 0 ${-2 * r} 0z"

private fun rect(x: Float, y: Float, w: Float, h: Float, r: Float): String =
    "M${x + r} ${y}h${w - 2 * r}a$r $r 0 0 1 $r ${r}v${h - 2 * r}a$r $r 0 0 1 ${-r} ${r}" +
        "h${-(w - 2 * r)}a$r $r 0 0 1 ${-r} ${-r}v${-(h - 2 * r)}a$r $r 0 0 1 $r ${-r}z"

private fun stroke(name: String, vararg paths: String, width: Float = 2f): ImageVector =
    ImageVector.Builder(name, 24.dp, 24.dp, 24f, 24f).apply {
        paths.forEach {
            addPath(
                pathData = addPathNodes(it),
                stroke = SolidColor(Color.Black),
                strokeLineWidth = width,
                strokeLineCap = StrokeCap.Round,
                strokeLineJoin = StrokeJoin.Round,
            )
        }
    }.build()

private fun fill(name: String, vararg paths: String): ImageVector =
    ImageVector.Builder(name, 24.dp, 24.dp, 24f, 24f).apply {
        paths.forEach { addPath(pathData = addPathNodes(it), fill = SolidColor(Color.Black)) }
    }.build()
