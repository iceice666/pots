package dev.pots.mobile.ui.screens

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
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
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.PathEffect
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.LocalNotice
import dev.pots.mobile.PotsNav
import dev.pots.mobile.Routes
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.model.ProjectView
import dev.pots.mobile.ui.components.CountBadge
import dev.pots.mobile.ui.components.GroupedList
import dev.pots.mobile.ui.components.HeaderBar
import dev.pots.mobile.ui.components.IconAction
import dev.pots.mobile.ui.components.IconTile
import dev.pots.mobile.ui.components.LetterAvatar
import dev.pots.mobile.ui.components.StatusLabel
import dev.pots.mobile.ui.components.avatar
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Pots

@Composable
fun ProjectsScreen(store: PotsStore, nav: PotsNav) {
    val notice = LocalNotice.current
    Column(Modifier.fillMaxSize().background(Pots.Bg)) {
        HeaderBar {
            Column(
                verticalArrangement = Arrangement.spacedBy(14.dp),
                modifier = Modifier.statusBarsPadding().padding(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 14.dp),
            ) {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    InstanceSwitcher(store)
                    Spacer(Modifier.weight(1f))
                    IconAction(PotsIcons.Search, "搜尋專案", { notice("搜尋還沒實作") })
                    IconAction(PotsIcons.Plus, "新增專案", { notice("新增專案還沒實作") })
                }
                Text("專案", style = MaterialTheme.typography.headlineMedium, color = Pots.Ink)
            }
        }

        LazyColumn(
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 14.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
            modifier = Modifier.fillMaxSize(),
        ) {
            store.inboxBanner?.let { (title, subtitle) ->
                item(key = "banner") { InboxBanner(title, subtitle) { nav.tab(Routes.INBOX) } }
            }
            item(key = "label") {
                Text(
                    "這個實例 · ${store.projects.size} 個專案",
                    fontSize = 13.sp,
                    fontWeight = FontWeight.SemiBold,
                    color = Pots.Muted,
                    modifier = Modifier.padding(start = 4.dp, end = 4.dp, top = 2.dp),
                )
            }
            item(key = "projects") {
                GroupedList(store.projects) { project ->
                    ProjectRow(project, pending = store.pendingFor(project.id)) { nav.channel(project.id) }
                }
            }
            items(store.handoffs.size, key = { "handoff-$it" }) { index ->
                val handoff = store.handoffs[index]
                HandoffRow("${handoff.project} 已移交到「${handoff.target}」，在桌面客戶端開啟")
            }
        }
    }
}

@Composable
private fun InstanceSwitcher(store: PotsStore) {
    val notice = LocalNotice.current
    var open by remember { mutableStateOf(false) }
    val instance = store.instance
    Box {
        OutlinedButton(
            onClick = { open = true },
            shape = RoundedCornerShape(12.dp),
            border = BorderStroke(1.dp, Pots.Border),
            contentPadding = PaddingValues(start = 6.dp, end = 12.dp),
            colors = ButtonDefaults.outlinedButtonColors(containerColor = Pots.Surface, contentColor = Pots.Ink),
            modifier = Modifier.heightIn(min = 44.dp),
        ) {
            IconTile(PotsIcons.Server)
            Column(Modifier.padding(start = 10.dp, end = 10.dp)) {
                Text(instance.name, fontSize = 14.sp, fontWeight = FontWeight.Bold, lineHeight = 17.sp)
                Text(
                    "${instance.kind.label} · ${instance.connection.label}",
                    fontSize = 12.sp,
                    color = Pots.Muted,
                    lineHeight = 15.sp,
                )
            }
            Icon(PotsIcons.ChevronDown, contentDescription = "切換實例", tint = Pots.Muted, modifier = Modifier.size(16.dp))
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }, containerColor = Pots.Surface) {
            store.instances.forEach { item ->
                DropdownMenuItem(
                    text = {
                        Column {
                            Text(item.name, fontWeight = FontWeight.SemiBold)
                            Text("${item.kind.label} · ${item.connection.label}", fontSize = 12.sp, color = Pots.Muted)
                        }
                    },
                    leadingIcon = { IconTile(PotsIcons.Server, size = 26.dp, iconSize = 14.dp) },
                    trailingIcon = {
                        if (item.id == store.currentInstance) Icon(PotsIcons.Check, contentDescription = "目前實例", tint = Pots.Accent, modifier = Modifier.size(18.dp))
                    },
                    onClick = {
                        store.selectInstance(item.id)
                        open = false
                    },
                )
            }
            HorizontalDivider(color = Pots.Divider)
            DropdownMenuItem(
                text = { Text("加入實例") },
                leadingIcon = { Icon(PotsIcons.Plus, contentDescription = null, modifier = Modifier.size(18.dp)) },
                onClick = {
                    open = false
                    notice("加入實例還沒實作")
                },
            )
        }
    }
}

@Composable
private fun InboxBanner(title: String, subtitle: String, onClick: () -> Unit) {
    val shape = RoundedCornerShape(14.dp)
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        modifier = Modifier
            .fillMaxWidth()
            .clip(shape)
            .background(Pots.WarnSoft)
            .border(1.dp, Pots.WarnBorder, shape)
            .clickable(role = Role.Button, onClick = onClick)
            .padding(14.dp),
    ) {
        Icon(PotsIcons.Warning, contentDescription = null, tint = Pots.WarnStrong, modifier = Modifier.size(22.dp))
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(title, fontSize = 15.sp, fontWeight = FontWeight.Bold, color = Pots.WarnStrong)
            Text(subtitle, fontSize = 13.sp, color = Pots.WarnStrong)
        }
        Icon(PotsIcons.ChevronRight, contentDescription = null, tint = Pots.WarnStrong, modifier = Modifier.size(18.dp))
    }
}

@Composable
private fun ProjectRow(project: ProjectView, pending: Int, onClick: () -> Unit) {
    val (fg, bg) = project.avatarTone.avatar()
    Row(
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(14.dp),
    ) {
        LetterAvatar(project.initial, fg, bg)
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(project.name, fontSize = 16.sp, fontWeight = FontWeight.Bold, color = Pots.Ink, modifier = Modifier.weight(1f))
                Text(project.lastActivity, fontSize = 12.sp, color = Pots.Muted)
            }
            StatusLabel(project.status)
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    project.summary,
                    fontSize = 14.sp,
                    color = Pots.Muted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (pending > 0) CountBadge(pending)
            }
        }
    }
}

@Composable
private fun HandoffRow(text: String) {
    val dash = remember { PathEffect.dashPathEffect(floatArrayOf(10f, 8f)) }
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        modifier = Modifier
            .fillMaxWidth()
            .drawBehind {
                val radius = 14.dp.toPx()
                drawRoundRect(
                    color = Pots.Control,
                    cornerRadius = androidx.compose.ui.geometry.CornerRadius(radius, radius),
                    topLeft = Offset.Zero,
                    style = Stroke(width = 1.dp.toPx(), pathEffect = dash),
                )
            }
            .padding(horizontal = 14.dp, vertical = 12.dp),
    ) {
        Icon(PotsIcons.ArrowRight, contentDescription = null, tint = Pots.Muted, modifier = Modifier.size(18.dp))
        Text(text, fontSize = 13.sp, color = Pots.Muted, lineHeight = 19.sp)
    }
}
