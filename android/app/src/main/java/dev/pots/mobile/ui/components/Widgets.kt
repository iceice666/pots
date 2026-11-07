package dev.pots.mobile.ui.components

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.model.InboxKind
import dev.pots.mobile.model.ProjectStatus
import dev.pots.mobile.model.Tone
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Pots

// ---------------------------------------------------------------------------
// 語意色

val Tone.fg: Color
    get() = when (this) {
        Tone.Accent -> Pots.AccentStrong
        Tone.Warn -> Pots.Warn
        Tone.Neutral -> Pots.Muted
        Tone.Success -> Pots.Success
        Tone.Violet -> Pots.Violet
    }

val Tone.dot: Color
    get() = when (this) {
        Tone.Accent -> Pots.Accent
        Tone.Warn -> Pots.Amber
        Tone.Neutral -> Pots.Idle
        Tone.Success -> Pots.Success
        Tone.Violet -> Pots.Violet
    }

/** 專案頭像的前景／背景。 */
fun Tone.avatar(): Pair<Color, Color> = when (this) {
    Tone.Accent -> Pots.AccentStrong to Pots.AccentSoft
    Tone.Warn -> Pots.WarnStrong to Pots.WarnSoft
    Tone.Violet -> Pots.Violet to Pots.VioletSoft
    Tone.Success -> Pots.Success to Pots.SuccessSoft
    Tone.Neutral -> Pots.InkSoft to Pots.Neutral
}

// ---------------------------------------------------------------------------
// 小元件

/** 方角小標籤：「核准」「GOAL」「CI 12/12」。 */
@Composable
fun Tag(text: String, fg: Color, bg: Color, modifier: Modifier = Modifier, fontSize: TextUnit = 12.sp) {
    Text(
        text = text,
        color = fg,
        fontSize = fontSize,
        fontWeight = FontWeight.Bold,
        lineHeight = fontSize * 1.3f,
        modifier = modifier
            .background(bg, RoundedCornerShape(4.dp))
            .padding(horizontal = 8.dp, vertical = 2.dp),
    )
}

@Composable
fun InboxKindTag(kind: InboxKind, short: Boolean = false, fontSize: TextUnit = 12.sp) = when (kind) {
    InboxKind.Approval -> Tag("核准", Pots.WarnStrong, Pots.WarnSoft, fontSize = fontSize)
    InboxKind.NeedsYou -> Tag(if (short) "需要你" else kind.label, Color.White, Pots.Warn, fontSize = fontSize)
}

/** 圓角數字徽章。 */
@Composable
fun CountBadge(count: Int, modifier: Modifier = Modifier, small: Boolean = false) {
    Box(
        contentAlignment = Alignment.Center,
        modifier = modifier
            .heightIn(min = if (small) 18.dp else 20.dp)
            .widthIn(min = if (small) 18.dp else 20.dp)
            .background(Pots.Warn, CircleShape)
            .padding(horizontal = if (small) 5.dp else 8.dp),
    ) {
        Text(
            text = count.toString(),
            color = Color.White,
            fontSize = if (small) 11.sp else 12.sp,
            fontWeight = FontWeight.Bold,
            lineHeight = 14.sp,
        )
    }
}

@Composable
fun StatusDot(color: Color, size: Dp = 7.dp) {
    Box(Modifier.size(size).background(color, CircleShape))
}

@Composable
fun StatusLabel(status: ProjectStatus, fontSize: TextUnit = 13.sp) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        StatusDot(status.tone.dot)
        Text(status.label, color = status.tone.fg, fontSize = fontSize, fontWeight = FontWeight.SemiBold)
    }
}

@Composable
fun LetterAvatar(letter: String, fg: Color, bg: Color, size: Dp = 40.dp, radius: Dp = 10.dp, fontSize: TextUnit = 15.sp) {
    Box(
        contentAlignment = Alignment.Center,
        modifier = Modifier.size(size).background(bg, RoundedCornerShape(radius)),
    ) {
        Text(letter, color = fg, fontSize = fontSize, fontWeight = FontWeight.Bold)
    }
}

/** 深色方塊裡放圖示，例如實例圖示與總管頭像。 */
@Composable
fun IconTile(icon: ImageVector, size: Dp = 30.dp, iconSize: Dp = 16.dp, radius: Dp = 8.dp) {
    Box(
        contentAlignment = Alignment.Center,
        modifier = Modifier.size(size).background(Pots.Ink, RoundedCornerShape(radius)),
    ) {
        Icon(icon, contentDescription = null, tint = Color.White, modifier = Modifier.size(iconSize))
    }
}

@Composable
fun SectionLabel(text: String, modifier: Modifier = Modifier) {
    Text(text, color = Pots.Muted, fontSize = 12.sp, fontWeight = FontWeight.Bold, modifier = modifier)
}

/** 44dp 的圖示按鈕；`label` 給讀屏器。 */
@Composable
fun IconAction(icon: ImageVector, label: String, onClick: () -> Unit, tint: Color = Pots.Ink, iconSize: Dp = 22.dp) {
    IconButton(
        onClick = onClick,
        modifier = Modifier.size(44.dp),
        colors = IconButtonDefaults.iconButtonColors(contentColor = tint),
    ) {
        Icon(icon, contentDescription = label, modifier = Modifier.size(iconSize))
    }
}

// ---------------------------------------------------------------------------
// 按鈕

@Composable
fun PrimaryButton(
    text: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    height: Dp = 48.dp,
    radius: Dp = 10.dp,
    fontSize: TextUnit = 16.sp,
    enabled: Boolean = true,
) {
    Button(
        onClick = onClick,
        enabled = enabled,
        modifier = modifier.height(height),
        shape = RoundedCornerShape(radius),
        contentPadding = PaddingValues(horizontal = 12.dp),
        colors = ButtonDefaults.buttonColors(containerColor = Pots.Accent, contentColor = Color.White),
    ) {
        Text(text, fontSize = fontSize, fontWeight = FontWeight.SemiBold, maxLines = 1)
    }
}

@Composable
fun SecondaryButton(
    text: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    height: Dp = 48.dp,
    radius: Dp = 10.dp,
    fontSize: TextUnit = 16.sp,
    color: Color = Pots.Ink,
    borderColor: Color = Pots.Control,
) {
    OutlinedButton(
        onClick = onClick,
        modifier = modifier.height(height),
        shape = RoundedCornerShape(radius),
        border = BorderStroke(1.dp, borderColor),
        contentPadding = PaddingValues(horizontal = 12.dp),
        colors = ButtonDefaults.outlinedButtonColors(containerColor = Pots.Surface, contentColor = color),
    ) {
        Text(text, fontSize = fontSize, fontWeight = FontWeight.SemiBold, maxLines = 1)
    }
}

/** 圓角 pill；Inbox 篩選、抽屜分頁、總管建議都用它。 */
@Composable
fun Pill(text: String, selected: Boolean, onClick: () -> Unit, modifier: Modifier = Modifier, role: Role = Role.Tab) {
    Box(
        contentAlignment = Alignment.Center,
        modifier = modifier
            .height(36.dp)
            .clip(CircleShape)
            .background(if (selected) Pots.Ink else Pots.Surface)
            .border(1.dp, if (selected) Pots.Ink else Pots.Control, CircleShape)
            .selectable(selected = selected, onClick = onClick, role = role)
            .padding(horizontal = 14.dp),
    ) {
        Text(
            text,
            color = if (selected) Color.White else Pots.Ink,
            fontSize = 13.sp,
            fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Normal,
        )
    }
}

/** 灰底分段切換：「排隊／改方向」。 */
@Composable
fun <T> Segmented(options: List<T>, selected: T, label: (T) -> String, onSelect: (T) -> Unit, description: String) {
    Row(
        modifier = Modifier
            .background(Pots.Neutral, RoundedCornerShape(10.dp))
            .padding(3.dp)
            .semantics { contentDescription = description }
            .selectableGroup(),
    ) {
        options.forEach { option ->
            val on = option == selected
            Box(
                contentAlignment = Alignment.Center,
                modifier = Modifier
                    .height(32.dp)
                    .clip(RoundedCornerShape(8.dp))
                    .background(if (on) Pots.Surface else Color.Transparent)
                    .selectable(selected = on, onClick = { onSelect(option) }, role = Role.RadioButton)
                    .padding(horizontal = 12.dp),
            ) {
                Text(label(option), color = if (on) Pots.Ink else Pots.InkSoft, fontSize = 13.sp, fontWeight = FontWeight.SemiBold)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 容器

/** 白底、1dp 邊框、14dp 圓角的卡片。 */
@Composable
fun PotsCard(
    modifier: Modifier = Modifier,
    borderColor: Color = Pots.Border,
    radius: Dp = 14.dp,
    onClick: (() -> Unit)? = null,
    contentPadding: PaddingValues = PaddingValues(16.dp),
    verticalSpacing: Dp = 10.dp,
    content: @Composable ColumnScope.() -> Unit,
) {
    val shape = RoundedCornerShape(radius)
    Column(
        verticalArrangement = Arrangement.spacedBy(verticalSpacing),
        modifier = modifier
            .fillMaxWidth()
            .clip(shape)
            .background(Pots.Surface)
            .border(1.dp, borderColor, shape)
            .then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier)
            .padding(contentPadding),
        content = content,
    )
}

/** 分組清單：外框圓角，列與列之間細線。 */
@Composable
fun <T> GroupedList(items: List<T>, modifier: Modifier = Modifier, radius: Dp = 14.dp, row: @Composable (T) -> Unit) {
    val shape = RoundedCornerShape(radius)
    Column(
        modifier = modifier
            .fillMaxWidth()
            .clip(shape)
            .background(Pots.Surface)
            .border(1.dp, Pots.Border, shape),
    ) {
        items.forEachIndexed { index, item ->
            if (index > 0) HorizontalDivider(color = Pots.Divider, thickness = 1.dp)
            row(item)
        }
    }
}

/** 頂端白色標題列，延伸到狀態列後方。 */
@Composable
fun HeaderBar(modifier: Modifier = Modifier, content: @Composable ColumnScope.() -> Unit) {
    Surface(color = Pots.Surface, modifier = modifier.fillMaxWidth()) {
        Column {
            Column(content = content)
            HorizontalDivider(color = Pots.Border, thickness = 1.dp)
        }
    }
}

// ---------------------------------------------------------------------------
// 輸入

/** 圓角輸入框加圓形送出鈕，對話與總管共用。 */
@Composable
fun Composer(
    value: String,
    onValueChange: (String) -> Unit,
    placeholder: String,
    onSend: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        modifier = modifier.fillMaxWidth(),
    ) {
        PotsTextField(
            value = value,
            onValueChange = onValueChange,
            placeholder = placeholder,
            radius = 22.dp,
            imeAction = ImeAction.Send,
            onIme = onSend,
            modifier = Modifier.weight(1f),
        )
        IconButton(
            onClick = onSend,
            modifier = Modifier.size(44.dp),
            colors = IconButtonDefaults.iconButtonColors(containerColor = Pots.Accent, contentColor = Color.White),
        ) {
            Icon(PotsIcons.ArrowUp, contentDescription = "送出", modifier = Modifier.size(20.dp))
        }
    }
}

@Composable
fun PotsTextField(
    value: String,
    onValueChange: (String) -> Unit,
    placeholder: String,
    modifier: Modifier = Modifier,
    radius: Dp = 10.dp,
    imeAction: ImeAction = ImeAction.Done,
    onIme: () -> Unit = {},
) {
    val shape = RoundedCornerShape(radius)
    BasicTextField(
        value = value,
        onValueChange = onValueChange,
        singleLine = true,
        textStyle = MaterialTheme.typography.bodyLarge.copy(color = Pots.Ink),
        cursorBrush = SolidColor(Pots.Accent),
        keyboardOptions = KeyboardOptions(imeAction = imeAction),
        keyboardActions = KeyboardActions(onAny = { onIme() }),
        modifier = modifier
            .height(44.dp)
            .semantics { contentDescription = placeholder },
        decorationBox = { inner ->
            Box(
                contentAlignment = Alignment.CenterStart,
                modifier = Modifier
                    .fillMaxWidth()
                    .height(44.dp)
                    .background(Pots.Surface, shape)
                    .border(1.dp, Pots.InputBorder, shape)
                    .padding(horizontal = 14.dp),
            ) {
                if (value.isEmpty()) {
                    Text(placeholder, color = Pots.Muted, fontSize = 15.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
                inner()
            }
        },
    )
}

/** 列表裡可點的一列：左圖示、標題與副標、右側內容。 */
@Composable
fun ListRow(
    title: String,
    subtitle: String?,
    onClick: (() -> Unit)?,
    modifier: Modifier = Modifier,
    leading: (@Composable () -> Unit)? = null,
    trailing: (@Composable RowScope.() -> Unit)? = null,
    minHeight: Dp = 60.dp,
) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        modifier = modifier
            .fillMaxWidth()
            .heightIn(min = minHeight)
            .then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier)
            .padding(horizontal = 12.dp, vertical = 10.dp),
    ) {
        leading?.invoke()
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(title, fontSize = 15.sp, fontWeight = FontWeight.SemiBold, color = Pots.Ink)
            if (subtitle != null) Text(subtitle, fontSize = 12.sp, color = Pots.Muted)
        }
        trailing?.invoke(this)
    }
}
