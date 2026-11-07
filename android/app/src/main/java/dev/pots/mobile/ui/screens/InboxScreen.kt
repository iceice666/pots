package dev.pots.mobile.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.LocalNotice
import dev.pots.mobile.PotsNav
import dev.pots.mobile.model.InboxItem
import dev.pots.mobile.model.InboxKind
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.model.ageLabel
import dev.pots.mobile.ui.components.ApprovalButtons
import dev.pots.mobile.ui.components.CountBadge
import dev.pots.mobile.ui.components.HeaderBar
import dev.pots.mobile.ui.components.InboxKindTag
import dev.pots.mobile.ui.components.Pill
import dev.pots.mobile.ui.components.PotsCard
import dev.pots.mobile.ui.components.PrimaryButton
import dev.pots.mobile.ui.components.SecondaryButton
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Pots

@Composable
fun InboxScreen(store: PotsStore, nav: PotsNav) {
    // null 表示「全部」。
    var filter by rememberSaveable { mutableStateOf<InboxKind?>(null) }
    val pending = store.pendingInbox
    val shown = pending.filter { filter == null || it.kind == filter }

    Column(Modifier.fillMaxSize().background(Pots.Bg)) {
        HeaderBar {
            Column(
                verticalArrangement = Arrangement.spacedBy(12.dp),
                modifier = Modifier.statusBarsPadding().padding(start = 16.dp, end = 16.dp, top = 20.dp, bottom = 12.dp),
            ) {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    Text("Inbox", style = MaterialTheme.typography.headlineMedium, color = Pots.Ink)
                    if (pending.isNotEmpty()) CountBadge(pending.size)
                    Spacer(Modifier.weight(1f))
                    Text(
                        "所有實例",
                        fontSize = 12.sp,
                        fontWeight = FontWeight.SemiBold,
                        color = Pots.InkSoft,
                        modifier = Modifier.background(Pots.Neutral, CircleShape).padding(horizontal = 10.dp, vertical = 4.dp),
                    )
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.selectableGroup()) {
                    Pill("全部", selected = filter == null, onClick = { filter = null })
                    Pill("核准 ${store.pendingCount(InboxKind.Approval)}", selected = filter == InboxKind.Approval, onClick = { filter = InboxKind.Approval })
                    Pill("需要你 ${store.pendingCount(InboxKind.NeedsYou)}", selected = filter == InboxKind.NeedsYou, onClick = { filter = InboxKind.NeedsYou })
                }
            }
        }

        LazyColumn(
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 14.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
            modifier = Modifier.fillMaxSize(),
        ) {
            if (shown.isEmpty()) {
                item(key = "empty") {
                    Text(
                        if (pending.isEmpty()) "都處理完了。" else "這個分類沒有待處理的項目。",
                        fontSize = 15.sp,
                        color = Pots.Muted,
                        modifier = Modifier.padding(vertical = 32.dp).fillMaxWidth(),
                    )
                }
            }
            items(shown, key = { it.id.raw }) { item -> InboxCard(store, nav, item) }
            item(key = "quiet") {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                    modifier = Modifier.padding(horizontal = 4.dp, vertical = 12.dp),
                ) {
                    Icon(PotsIcons.Moon, contentDescription = null, tint = Pots.Muted, modifier = Modifier.size(16.dp))
                    Text("靜音時段 23:00–08:00，期間的請求會合併成一則推播", fontSize = 13.sp, color = Pots.Muted, lineHeight = 19.sp)
                }
            }
        }
    }
}

@Composable
private fun InboxCard(store: PotsStore, nav: PotsNav, item: InboxItem) {
    val notice = LocalNotice.current
    val project = store.project(item.project)
    PotsCard(borderColor = if (item.kind == InboxKind.NeedsYou) Pots.WarnBorder else Pots.Border) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            InboxKindTag(item.kind, fontSize = 13.sp)
            Text("# ${project.name}", fontSize = 13.sp, fontWeight = FontWeight.SemiBold, color = Pots.Ink)
            Spacer(Modifier.weight(1f))
            Text(ageLabel(item.ageMinutes), fontSize = 13.sp, color = Pots.Muted)
        }
        Text(item.title, fontSize = 17.sp, fontWeight = FontWeight.Bold, lineHeight = 24.sp, color = Pots.Ink)
        Text(item.detail, fontSize = 14.sp, color = Pots.Muted, lineHeight = 21.sp)
        when (item.kind) {
            InboxKind.Approval -> {
                val id = item.approval ?: return@PotsCard
                ApprovalButtons(store, id, modifier = Modifier.padding(top = 4.dp))
                TextButton(
                    onClick = { nav.approval(id) },
                    modifier = Modifier.align(Alignment.CenterHorizontally).heightIn(min = 44.dp),
                ) {
                    Text(
                        if (store.approval(id)?.files?.isNotEmpty() == true) "看 diff 與詳情" else "看詳情",
                        color = Pots.Accent,
                        fontSize = 14.sp,
                        fontWeight = FontWeight.SemiBold,
                    )
                }
            }
            InboxKind.NeedsYou -> Row(horizontalArrangement = Arrangement.spacedBy(10.dp), modifier = Modifier.padding(top = 4.dp)) {
                PrimaryButton(item.primaryAction ?: "處理", onClick = {
                    store.retry(item.id)
                    notice("已重新觸發：${project.name}")
                }, modifier = Modifier.weight(1f))
                SecondaryButton("看 thread", onClick = { nav.channel(item.project) }, modifier = Modifier.weight(1f))
            }
        }
    }
}
