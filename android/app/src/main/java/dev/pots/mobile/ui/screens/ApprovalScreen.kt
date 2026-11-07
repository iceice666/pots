package dev.pots.mobile.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.LocalNotice
import dev.pots.mobile.PotsNav
import dev.pots.mobile.model.ApprovalId
import dev.pots.mobile.model.FileChange
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.ui.components.ApprovalButtons
import dev.pots.mobile.ui.components.IconAction
import dev.pots.mobile.ui.components.PotsTextField
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Mono
import dev.pots.mobile.ui.theme.Pots

/** 推播落地的核准專頁：要被執行的動作、檢查結果、diff 摘要，可附理由。 */
@Composable
fun ApprovalScreen(store: PotsStore, nav: PotsNav, id: ApprovalId) {
    val notice = LocalNotice.current
    val approval = store.approval(id) ?: return MissingApproval(nav)
    val project = store.project(approval.project)
    var note by rememberSaveable { mutableStateOf("") }

    Column(Modifier.fillMaxSize().background(Pots.Surface)) {
        ApprovalHeader(chip = store.instance.name, chipFg = Pots.InkSoft, chipBg = Pots.Neutral, onClose = nav::back)

        Column(
            verticalArrangement = Arrangement.spacedBy(16.dp),
            modifier = Modifier
                .weight(1f)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 16.dp, vertical = 18.dp),
        ) {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("# ${project.name}", fontSize = 13.sp, fontWeight = FontWeight.SemiBold, color = Pots.Ink)
                    approval.goal?.let { Text("· Goal $it", fontSize = 13.sp, color = Pots.Muted) }
                }
                Text(approval.title, style = MaterialTheme.typography.headlineSmall, color = Pots.Ink)
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    Icon(PotsIcons.Clock, contentDescription = null, tint = Pots.Warn, modifier = Modifier.size(14.dp))
                    Text("剩 ${approval.expiresInHours} 小時 · 逾時轉為需要你處理", fontSize = 13.sp, color = Pots.Warn, fontWeight = FontWeight.SemiBold)
                }
            }

            Column(
                verticalArrangement = Arrangement.spacedBy(10.dp),
                modifier = Modifier
                    .fillMaxWidth()
                    .background(Pots.SurfaceSunken, RoundedCornerShape(12.dp))
                    .border(1.dp, Pots.Border, RoundedCornerShape(12.dp))
                    .padding(14.dp),
            ) {
                Text("將被執行的動作", fontSize = 12.sp, fontWeight = FontWeight.Bold, color = Pots.Muted)
                approval.action.forEach { field ->
                    Row {
                        Text(field.label, fontSize = 14.sp, color = Pots.Muted, modifier = Modifier.width(72.dp))
                        Text(
                            field.value,
                            fontSize = if (field.mono) 13.sp else 14.sp,
                            fontFamily = if (field.mono) Mono else null,
                            color = Pots.Ink,
                            modifier = Modifier.weight(1f),
                        )
                    }
                }
                HorizontalDivider(color = Pots.Border)
                Text("由平台執行，內容與核准時一致才會${if (approval.files.isNotEmpty()) " merge" else "生效"}。", fontSize = 12.sp, color = Pots.Muted, lineHeight = 18.sp)
            }

            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                approval.checks.forEach { check ->
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        Icon(PotsIcons.Check, contentDescription = null, tint = Pots.Success, modifier = Modifier.size(18.dp))
                        Text(check, fontSize = 14.sp, color = Pots.Ink)
                    }
                }
            }

            if (approval.files.isNotEmpty()) {
                DiffSummary(approval.files, approval.totalFiles, approval.added, approval.removed) { notice("完整 diff 還沒實作") }
            }

            Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text("附一句理由（選填）", fontSize = 13.sp, fontWeight = FontWeight.SemiBold, color = Pots.Ink)
                PotsTextField(note, { note = it }, placeholder = "拒絕時會帶給 agent", modifier = Modifier.fillMaxWidth())
            }
        }

        DecisionBar {
            ApprovalButtons(store, id, height = 54.dp, approveLabel = approval.confirmLabel, note = { note }, reversed = true)
        }
    }
}

@Composable
private fun DiffSummary(files: List<FileChange>, total: Int, added: Int, removed: Int, onOpen: () -> Unit) {
    val shape = RoundedCornerShape(12.dp)
    Column(Modifier.fillMaxWidth().clip(shape).border(1.dp, Pots.Border, shape).clickable(onClick = onOpen)) {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.padding(horizontal = 12.dp, vertical = 10.dp)) {
            Text("$total 個檔案", fontSize = 13.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
            Text("+$added", fontSize = 13.sp, fontWeight = FontWeight.SemiBold, color = Pots.Success)
            Text("−$removed", fontSize = 13.sp, fontWeight = FontWeight.SemiBold, color = Pots.Danger)
            Spacer(Modifier.weight(1f))
            Text("看完整 diff", fontSize = 13.sp, fontWeight = FontWeight.SemiBold, color = Pots.Accent)
        }
        files.take(2).forEach { file ->
            HorizontalDivider(color = Pots.Divider)
            DiffLine {
                Text(file.path, fontFamily = Mono, fontSize = 12.sp, color = Pots.Ink, modifier = Modifier.weight(1f))
                if (file.added > 0) Text("+${file.added}", fontFamily = Mono, fontSize = 12.sp, color = Pots.Success)
                if (file.removed > 0) Text("−${file.removed}", fontFamily = Mono, fontSize = 12.sp, color = Pots.Danger)
            }
        }
        if (files.size > 2) {
            HorizontalDivider(color = Pots.Divider)
            DiffLine { Text("另外 ${files.size - 2} 個檔案", fontSize = 12.sp, color = Pots.Muted) }
        }
    }
}

@Composable
private fun DiffLine(content: @Composable RowScope.() -> Unit) {
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 9.dp), content = content)
}

@Composable
internal fun ApprovalHeader(chip: String, chipFg: Color, chipBg: Color, onClose: () -> Unit) {
    Column(Modifier.background(Pots.Surface)) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            modifier = Modifier.statusBarsPadding().padding(start = 16.dp, end = 8.dp, top = 8.dp, bottom = 8.dp),
        ) {
            Text("核准請求", fontSize = 15.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
            Text(
                chip,
                fontSize = 12.sp,
                fontWeight = FontWeight.SemiBold,
                color = chipFg,
                modifier = Modifier.background(chipBg, CircleShape).padding(horizontal = 8.dp, vertical = 3.dp),
            )
            Spacer(Modifier.weight(1f))
            IconAction(PotsIcons.Close, "關閉", onClose)
        }
        HorizontalDivider(color = Pots.Border)
    }
}

/** 底部固定的決定列；鍵盤打開時跟著上移。 */
@Composable
internal fun DecisionBar(content: @Composable () -> Unit) {
    Column(Modifier.background(Pots.Surface)) {
        HorizontalDivider(color = Pots.Border)
        Box(
            Modifier
                .windowInsetsPadding(WindowInsets.ime.union(WindowInsets.navigationBars))
                .padding(start = 16.dp, end = 16.dp, top = 12.dp, bottom = 16.dp),
        ) { content() }
    }
}

@Composable
internal fun MissingApproval(nav: PotsNav) {
    Column(Modifier.fillMaxSize().background(Pots.Surface)) {
        ApprovalHeader(chip = "找不到", chipFg = Pots.InkSoft, chipBg = Pots.Neutral, onClose = nav::back)
        Text("這筆核准已不存在，可能已在其他裝置處理。", fontSize = 15.sp, color = Pots.Muted, modifier = Modifier.padding(16.dp))
    }
}
