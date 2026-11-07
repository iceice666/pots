package dev.pots.mobile.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.Typography
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * 色票，數值取自手機設計稿（Pots 手機端 UI canvas）。
 *
 * 命名依用途而不是色相，方便之後加深色主題時只換這一份。
 */
object Pots {
    val Ink = Color(0xFF1F2328)
    val InkSoft = Color(0xFF3D434C)
    val Muted = Color(0xFF5C6370)
    val Idle = Color(0xFF9A9EA7)

    val Bg = Color(0xFFF7F8FA)
    val Surface = Color(0xFFFFFFFF)
    val SurfaceSunken = Color(0xFFF6F7F9)
    val Border = Color(0xFFE4E6EA)
    val Divider = Color(0xFFECEEF1)
    val Control = Color(0xFFD0D4DA)
    val InputBorder = Color(0xFFC9CDD3)
    val Neutral = Color(0xFFECEEF1)

    val Accent = Color(0xFF2F5BD3)
    val AccentStrong = Color(0xFF1E3F9E)
    val AccentSoft = Color(0xFFEEF3FF)

    val Warn = Color(0xFFB45309)
    val WarnStrong = Color(0xFF92400E)
    val WarnSoft = Color(0xFFFFF4E5)
    val WarnBorder = Color(0xFFE9B673)
    val Amber = Color(0xFFD97706)

    val Success = Color(0xFF15803D)
    val SuccessSoft = Color(0xFFECFDF3)

    val Danger = Color(0xFFB42318)
    val DangerBorder = Color(0xFFE3B4B4)

    val Violet = Color(0xFF6D4FC2)
    val VioletSoft = Color(0xFFF3F0FF)

    /** 狀態抽屜後方的遮罩。 */
    val Scrim = Color(0xFF3A3D44)
}

/** 設計稿用 JetBrains Mono 顯示雜湊、檔名；先用系統等寬字型。 */
val Mono: FontFamily = FontFamily.Monospace

private val ColorScheme = lightColorScheme(
    primary = Pots.Accent,
    onPrimary = Color.White,
    primaryContainer = Pots.AccentSoft,
    onPrimaryContainer = Pots.AccentStrong,
    secondary = Pots.Ink,
    onSecondary = Color.White,
    tertiary = Pots.Violet,
    background = Pots.Bg,
    onBackground = Pots.Ink,
    surface = Pots.Surface,
    onSurface = Pots.Ink,
    surfaceVariant = Pots.Neutral,
    onSurfaceVariant = Pots.Muted,
    surfaceContainerLow = Pots.Surface,
    surfaceContainer = Pots.Surface,
    surfaceContainerHigh = Pots.Surface,
    outline = Pots.Control,
    outlineVariant = Pots.Border,
    error = Pots.Danger,
    scrim = Pots.Scrim,
)

private fun style(size: Int, weight: FontWeight = FontWeight.Normal, line: Float = 1.4f) = TextStyle(
    fontSize = size.sp,
    fontWeight = weight,
    lineHeight = (size * line).sp,
    color = Color.Unspecified,
)

/** 字級對應設計稿：28 頁標題、17 卡片標題、15 內文、13 次要、12 標籤。 */
private val PotsTypography = Typography(
    headlineMedium = style(28, FontWeight.Bold, 1.25f).copy(letterSpacing = (-0.28).sp),
    headlineSmall = style(24, FontWeight.Bold, 1.3f),
    titleLarge = style(20, FontWeight.Bold, 1.3f),
    titleMedium = style(17, FontWeight.Bold),
    titleSmall = style(15, FontWeight.Bold),
    bodyLarge = style(15, line = 1.55f),
    bodyMedium = style(14, line = 1.5f),
    bodySmall = style(13, line = 1.5f),
    labelLarge = style(16, FontWeight.SemiBold),
    labelMedium = style(13, FontWeight.SemiBold),
    labelSmall = style(12, FontWeight.Bold),
)

private val PotsShapes = Shapes(
    extraSmall = RoundedCornerShape(4.dp),
    small = RoundedCornerShape(10.dp),
    medium = RoundedCornerShape(14.dp),
    large = RoundedCornerShape(20.dp),
    extraLarge = RoundedCornerShape(28.dp),
)

/** 設計稿只有淺色；深色主題等色票定案後再補。 */
@Composable
fun PotsTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = ColorScheme,
        typography = PotsTypography,
        shapes = PotsShapes,
        content = content,
    )
}
