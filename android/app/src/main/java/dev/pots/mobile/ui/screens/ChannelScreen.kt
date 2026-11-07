package dev.pots.mobile.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectVerticalDragGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.LocalNotice
import dev.pots.mobile.PotsNav
import dev.pots.mobile.model.ApprovalId
import dev.pots.mobile.model.CiState
import dev.pots.mobile.model.GoalView
import dev.pots.mobile.model.MsgStatus
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.model.ProjectId
import dev.pots.mobile.model.SendMode
import dev.pots.mobile.model.StreamItem
import dev.pots.mobile.model.TaskBucket
import dev.pots.mobile.ui.components.ApprovalButtons
import dev.pots.mobile.ui.components.Composer
import dev.pots.mobile.ui.components.IconAction
import dev.pots.mobile.ui.components.LetterAvatar
import dev.pots.mobile.ui.components.PotsCard
import dev.pots.mobile.ui.components.SecondaryButton
import dev.pots.mobile.ui.components.Segmented
import dev.pots.mobile.ui.components.StatusDot
import dev.pots.mobile.ui.components.Tag
import dev.pots.mobile.ui.components.dot
import dev.pots.mobile.ui.components.fg
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Pots
import java.time.LocalTime
import java.time.format.DateTimeFormatter

private val clock = DateTimeFormatter.ofPattern("HH:mm")

@Composable
fun ChannelScreen(store: PotsStore, nav: PotsNav, projectId: ProjectId, initiallyShowStatus: Boolean = false) {
    val notice = LocalNotice.current
    val project = store.project(projectId)
    val items = store.stream(projectId)
    val busy = store.isAgentBusy(projectId)
    val status = store.status(projectId)
    var sheetOpen by rememberSaveable { mutableStateOf(initiallyShowStatus) }
    var draft by rememberSaveable { mutableStateOf("") }
    val listState = rememberLazyListState()

    LaunchedEffect(items.size) {
        if (items.isNotEmpty()) listState.animateScrollToItem(items.lastIndex + if (busy) 1 else 0)
    }

    Column(Modifier.fillMaxSize().background(Pots.Surface)) {
        ChannelHeader(
            name = project.name,
            statusLabel = project.status.label,
            statusColor = project.status.tone.dot,
            statusText = project.status.tone.fg,
            budget = project.weekBudget.label,
            busy = busy,
            onBack = nav::back,
            onStop = {
                store.stop(projectId)
                notice("已送出停止")
            },
        )

        LazyColumn(
            state = listState,
            contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 0.dp, bottom = 14.dp),
            modifier = Modifier.weight(1f).fillMaxWidth().background(Pots.Bg),
        ) {
            itemsIndexed(items) { index, item ->
                val previous = items.getOrNull(index - 1)
                val continuesGroup = item.isAgentSide && previous?.isAgentSide == true
                Box(Modifier.padding(top = if (continuesGroup) 8.dp else 14.dp)) {
                    when {
                        item.isAgentSide -> AgentRow(project.name.take(1).uppercase(), showAvatar = !continuesGroup) {
                            AgentItem(item, onOpenGoal = { sheetOpen = true })
                        }
                        else -> StandaloneItem(store, nav, item)
                    }
                }
            }
            if (busy) {
                item(key = "typing") { TypingIndicator("${project.name} agent", Modifier.padding(top = 14.dp)) }
            }
        }

        HorizontalDivider(color = Pots.Border)
        StatusPeek(summary = statusSummary(status.goal), onOpen = { sheetOpen = true })

        HorizontalDivider(color = Pots.Border)
        Column(
            verticalArrangement = Arrangement.spacedBy(8.dp),
            modifier = Modifier
                .windowInsetsPadding(WindowInsets.ime.union(WindowInsets.navigationBars))
                .padding(start = 12.dp, end = 12.dp, top = 8.dp, bottom = 12.dp),
        ) {
            Composer(
                value = draft,
                onValueChange = { draft = it },
                placeholder = "傳訊息給 ${project.name} agent",
                onSend = {
                    store.send(projectId, draft, LocalTime.now().format(clock))
                    draft = ""
                },
            )
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Segmented(
                    options = SendMode.entries,
                    selected = store.sendMode(projectId),
                    label = { it.label },
                    onSelect = { store.setSendMode(projectId, it) },
                    description = "agent 忙碌時",
                )
                Text("/fix · /goal · /schedule", fontSize = 12.sp, color = Pots.Muted)
            }
        }
    }

    if (sheetOpen) {
        StatusSheet(store, nav, projectId, onDismiss = { sheetOpen = false })
    }
}

private fun statusSummary(goal: GoalView?): String {
    if (goal == null) return "沒有進行中的 Goal"
    val parts = buildList {
        add("${goal.count(TaskBucket.Running)} 進行中")
        goal.prs.firstNotNullOfOrNull { pr -> (pr.ci as? CiState.Running)?.let { "PR #${pr.number} CI ${it.done} / ${it.total}" } }?.let(::add)
    }
    return parts.joinToString(" · ")
}

@Composable
private fun ChannelHeader(
    name: String,
    statusLabel: String,
    statusColor: Color,
    statusText: Color,
    budget: String,
    busy: Boolean,
    onBack: () -> Unit,
    onStop: () -> Unit,
) {
    val notice = LocalNotice.current
    var menu by remember { mutableStateOf(false) }
    Column(Modifier.background(Pots.Surface)) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            modifier = Modifier.statusBarsPadding().padding(start = 8.dp, end = 8.dp, top = 8.dp, bottom = 10.dp),
        ) {
            IconAction(PotsIcons.ChevronLeft, "返回專案列表", onBack)
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
                Text("# $name", fontSize = 17.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                        StatusDot(statusColor)
                        Text(statusLabel, fontSize = 12.sp, color = statusText, fontWeight = FontWeight.SemiBold)
                    }
                    Text("·", fontSize = 12.sp, color = Pots.Muted)
                    Text("本週 $budget", fontSize = 12.sp, color = Pots.Muted)
                }
            }
            if (busy) {
                SecondaryButton(
                    "停止",
                    onClick = onStop,
                    height = 36.dp,
                    radius = 8.dp,
                    fontSize = 13.sp,
                    color = Pots.Danger,
                    borderColor = Pots.DangerBorder,
                    modifier = Modifier.semantics { contentDescription = "停止目前執行" },
                )
            }
            Box {
                IconAction(PotsIcons.More, "更多選項", { menu = true }, iconSize = 20.dp)
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }, containerColor = Pots.Surface) {
                    listOf("Artifacts", "Project 設定", "移交到其他實例").forEach { label ->
                        DropdownMenuItem(text = { Text(label) }, onClick = {
                            menu = false
                            notice("「$label」還沒實作")
                        })
                    }
                }
            }
        }
        HorizontalDivider(color = Pots.Border)
    }
}

/** agent 這側：頭像欄加內容，連續項目只在第一則畫頭像。 */
@Composable
private fun AgentRow(initial: String, showAvatar: Boolean, content: @Composable () -> Unit) {
    Row(horizontalArrangement = Arrangement.spacedBy(10.dp), modifier = Modifier.fillMaxWidth()) {
        if (showAvatar) {
            LetterAvatar(initial, Color.White, Pots.Ink, size = 30.dp, radius = 8.dp, fontSize = 12.sp)
        } else {
            Spacer(Modifier.width(30.dp))
        }
        Box(Modifier.weight(1f)) { content() }
    }
}

@Composable
private fun AgentItem(item: StreamItem, onOpenGoal: () -> Unit) {
    when (item) {
        is StreamItem.AgentMsg -> Text(item.text, fontSize = 15.sp, lineHeight = 23.sp, color = Pots.Ink)
        is StreamItem.GoalCard -> GoalCard(item.goal, onOpenGoal)
        is StreamItem.PrEvent -> Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Icon(PotsIcons.GitBranch, contentDescription = null, tint = Pots.Success, modifier = Modifier.size(16.dp))
            Text("${item.text} · ${item.time}", fontSize = 13.sp, color = Pots.Muted)
        }
        else -> Unit
    }
}

@Composable
private fun StandaloneItem(store: PotsStore, nav: PotsNav, item: StreamItem) {
    when (item) {
        is StreamItem.Away -> PotsCard(contentPadding = PaddingValues(horizontal = 14.dp, vertical = 12.dp), verticalSpacing = 8.dp) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Icon(PotsIcons.Clock, contentDescription = null, tint = Pots.Muted, modifier = Modifier.size(14.dp))
                Text("你不在期間 · ${item.duration}", fontSize = 12.sp, fontWeight = FontWeight.Bold, color = Pots.Muted)
            }
            Text(item.text, fontSize = 14.sp, lineHeight = 22.sp, color = Pots.Ink)
        }
        is StreamItem.UserMsg -> UserBubble(item)
        is StreamItem.ApprovalCard -> ChannelApprovalCard(store, nav, item.approval)
        else -> Unit
    }
}

@Composable
private fun UserBubble(msg: StreamItem.UserMsg) {
    BoxWithConstraints(Modifier.fillMaxWidth()) {
        val maxBubble = maxWidth * 0.82f
        Column(horizontalAlignment = Alignment.End, verticalArrangement = Arrangement.spacedBy(4.dp), modifier = Modifier.fillMaxWidth()) {
            Text(
                msg.text,
                color = Color.White,
                fontSize = 15.sp,
                lineHeight = 22.sp,
                modifier = Modifier
                    .widthIn(max = maxBubble)
                    .background(Pots.Accent, RoundedCornerShape(16.dp, 16.dp, 4.dp, 16.dp))
                    .padding(horizontal = 14.dp, vertical = 10.dp),
            )
            val caption = when (msg.status) {
                MsgStatus.Sent -> msg.time
                MsgStatus.Queued -> "排隊中 · ${msg.time}"
                MsgStatus.Steering -> "改方向 · ${msg.time}"
            }
            Text(caption, fontSize = 11.sp, color = Pots.Muted)
        }
    }
}

@Composable
private fun GoalCard(goal: GoalView, onClick: () -> Unit) {
    val done = goal.count(TaskBucket.Done)
    val running = goal.count(TaskBucket.Running)
    val waiting = goal.count(TaskBucket.Waiting)
    PotsCard(onClick = onClick, contentPadding = PaddingValues(horizontal = 14.dp, vertical = 12.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Tag("GOAL", Pots.Violet, Pots.VioletSoft, fontSize = 11.sp)
            Text(goal.title, fontSize = 15.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
        }
        Row(
            horizontalArrangement = Arrangement.spacedBy(2.dp),
            modifier = Modifier.fillMaxWidth().height(6.dp).clip(RoundedCornerShape(3.dp)).background(Pots.Neutral),
        ) {
            if (done > 0) Box(Modifier.weight(done.toFloat()).fillMaxSize().background(Pots.Success))
            if (running > 0) Box(Modifier.weight(running.toFloat()).fillMaxSize().background(Pots.Accent))
            if (waiting > 0) Box(Modifier.weight(waiting.toFloat()).fillMaxSize().background(Pots.Amber))
        }
        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Text("$done 完成", fontSize = 12.sp, color = Pots.Muted)
            Text("$running 進行中", fontSize = 12.sp, color = Pots.Muted)
            Text("$waiting 等待 CI", fontSize = 12.sp, color = Pots.Muted)
            Spacer(Modifier.weight(1f))
            Text("看任務", fontSize = 12.sp, color = Pots.Accent, fontWeight = FontWeight.SemiBold)
        }
    }
}

@Composable
private fun ChannelApprovalCard(store: PotsStore, nav: PotsNav, id: ApprovalId) {
    val approval = store.approval(id) ?: return
    PotsCard(borderColor = Pots.WarnBorder, contentPadding = PaddingValues(14.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Tag("需要你核准", Pots.WarnStrong, Pots.WarnSoft)
            Spacer(Modifier.weight(1f))
            Text("剩 ${approval.expiresInHours} 小時", fontSize = 12.sp, color = Pots.Muted)
        }
        Text(approval.title, fontSize = 16.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
        Text(approval.summary, fontSize = 13.sp, color = Pots.Muted, lineHeight = 20.sp)
        ApprovalButtons(store, id, height = 44.dp) {
            Box(
                contentAlignment = Alignment.Center,
                modifier = Modifier
                    .size(44.dp)
                    .clip(RoundedCornerShape(10.dp))
                    .border(1.dp, Pots.Control, RoundedCornerShape(10.dp))
                    .clickable(role = Role.Button) { nav.approval(id) }
                    .semantics { contentDescription = "看核准詳情" },
            ) {
                Icon(PotsIcons.ChevronRight, contentDescription = null, tint = Pots.Ink, modifier = Modifier.size(18.dp))
            }
        }
    }
}

@Composable
private fun TypingIndicator(who: String, modifier: Modifier = Modifier) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = modifier) {
        Row(horizontalArrangement = Arrangement.spacedBy(3.dp)) {
            repeat(3) { Box(Modifier.size(5.dp).background(Pots.Idle, CircleShape)) }
        }
        Text("$who 正在輸入…", fontSize = 13.sp, color = Pots.Muted)
    }
}

/** 對話頁底部的抽屜把手：點一下或往上拉都會打開狀態抽屜。 */
@Composable
private fun StatusPeek(summary: String, onOpen: () -> Unit) {
    Column(
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(10.dp),
        modifier = Modifier
            .fillMaxWidth()
            .background(Pots.Surface)
            .pointerInput(Unit) {
                detectVerticalDragGestures { change, dragAmount ->
                    if (dragAmount < -6f) {
                        change.consume()
                        onOpen()
                    }
                }
            }
            .clickable(role = Role.Button, onClickLabel = "打開狀態抽屜", onClick = onOpen)
            .padding(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 10.dp),
    ) {
        Box(Modifier.width(36.dp).height(4.dp).background(Pots.Control, RoundedCornerShape(2.dp)))
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp), modifier = Modifier.fillMaxWidth()) {
            Text("狀態", fontSize = 13.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
            Text(summary, fontSize = 13.sp, color = Pots.Muted, modifier = Modifier.weight(1f))
            Icon(PotsIcons.ChevronUp, contentDescription = null, tint = Pots.Muted, modifier = Modifier.size(18.dp))
        }
    }
}
