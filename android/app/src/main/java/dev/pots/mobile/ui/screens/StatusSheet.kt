package dev.pots.mobile.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.BottomSheetDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.LocalNotice
import dev.pots.mobile.PotsNav
import dev.pots.mobile.model.AutomationView
import dev.pots.mobile.model.CiState
import dev.pots.mobile.model.ComputerView
import dev.pots.mobile.model.ConnectionState
import dev.pots.mobile.model.GoalView
import dev.pots.mobile.model.Observation
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.model.PrView
import dev.pots.mobile.model.ProjectId
import dev.pots.mobile.model.TaskBucket
import dev.pots.mobile.model.TaskView
import dev.pots.mobile.ui.components.GroupedList
import dev.pots.mobile.ui.components.ListRow
import dev.pots.mobile.ui.components.Pill
import dev.pots.mobile.ui.components.PotsCard
import dev.pots.mobile.ui.components.PrimaryButton
import dev.pots.mobile.ui.components.SecondaryButton
import dev.pots.mobile.ui.components.SectionLabel
import dev.pots.mobile.ui.components.StatusDot
import dev.pots.mobile.ui.components.Tag
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Pots

enum class StatusTab { Tasks, Memory, Automations, Computers }

/** 對話頁的上拉抽屜：Goal、任務、PR 與 CI，以及記憶、自動化、電腦分頁。 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun StatusSheet(store: PotsStore, nav: PotsNav, projectId: ProjectId, onDismiss: () -> Unit) {
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = sheetState,
        containerColor = Pots.Surface,
        scrimColor = Pots.Scrim.copy(alpha = 0.88f),
        shape = RoundedCornerShape(topStart = 20.dp, topEnd = 20.dp),
        dragHandle = { BottomSheetDefaults.DragHandle(color = Pots.Control, width = 36.dp, height = 4.dp) },
    ) {
        StatusContent(store, nav, projectId, Modifier.fillMaxHeight(0.92f))
    }
}

@Composable
fun StatusContent(store: PotsStore, nav: PotsNav, projectId: ProjectId, modifier: Modifier = Modifier) {
    val status = store.status(projectId)
    var tab by rememberSaveable { mutableStateOf(StatusTab.Tasks) }
    Column(modifier.fillMaxWidth()) {
        Row(
            horizontalArrangement = Arrangement.spacedBy(6.dp),
            modifier = Modifier
                .horizontalScroll(rememberScrollState())
                .padding(start = 16.dp, end = 16.dp, top = 4.dp, bottom = 12.dp)
                .selectableGroup(),
        ) {
            StatusTab.entries.forEach { entry ->
                val label = when (entry) {
                    StatusTab.Tasks -> "任務"
                    StatusTab.Memory -> "記憶"
                    StatusTab.Automations -> "自動化" + status.automations.size.takeIf { it > 0 }?.let { " $it" }.orEmpty()
                    StatusTab.Computers -> "電腦"
                }
                Pill(label, selected = tab == entry, onClick = { tab = entry })
            }
        }
        HorizontalDivider(color = Pots.Border)
        Column(
            verticalArrangement = Arrangement.spacedBy(16.dp),
            modifier = Modifier
                .weight(1f)
                .verticalScroll(rememberScrollState())
                .navigationBarsPadding()
                .padding(start = 16.dp, end = 16.dp, top = 14.dp, bottom = 24.dp),
        ) {
            when (tab) {
                StatusTab.Tasks -> status.goal?.let { TasksTab(store, nav, it) } ?: EmptyNote("目前沒有進行中的 Goal。在對話裡輸入 /goal 開一個。")
                StatusTab.Memory -> MemoryTab(status.memory)
                StatusTab.Automations -> AutomationsTab(status.automations)
                StatusTab.Computers -> ComputersTab(status.computers)
            }
        }
    }
}

@Composable
private fun EmptyNote(text: String) {
    Text(text, fontSize = 14.sp, color = Pots.Muted, lineHeight = 21.sp, modifier = Modifier.padding(vertical = 24.dp))
}

// ---------------------------------------------------------------------------
// 任務

@Composable
private fun TasksTab(store: PotsStore, nav: PotsNav, goal: GoalView) {
    val notice = LocalNotice.current
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Tag("GOAL", Pots.Violet, Pots.VioletSoft, fontSize = 11.sp)
            Text(goal.title, fontSize = 17.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
        }
        Row {
            Text(goal.planner, fontSize = 13.sp, color = Pots.Muted)
            Spacer(Modifier.weight(1f))
            Text("預算 ${goal.budget.label}", fontSize = 13.sp, color = Pots.Muted)
        }
        Box(Modifier.fillMaxWidth().height(6.dp).clip(RoundedCornerShape(3.dp)).background(Pots.Neutral)) {
            Box(Modifier.fillMaxWidth(goal.progress).height(6.dp).background(Pots.Accent))
        }
    }

    TaskSection(goal, TaskBucket.Running)
    TaskSection(goal, TaskBucket.Waiting)

    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        SectionLabel("PR 與 CI")
        GroupedList(goal.prs, radius = 12.dp) { pr -> PrRow(store, pr) { pr.approval?.let(nav::approval) ?: notice("PR #${pr.number} 詳情還沒實作") } }
    }

    val done = goal.tasks.filter { it.bucket == TaskBucket.Done }
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Icon(PotsIcons.Check, contentDescription = null, tint = Pots.Success, modifier = Modifier.size(16.dp))
        Text("完成 ${done.size} · ${done.joinToString("、") { it.title }}", fontSize = 13.sp, color = Pots.Muted, modifier = Modifier.weight(1f))
        TextButton(onClick = { notice("取消 Goal 需要再確認一次，還沒實作") }) {
            Text("取消 Goal", color = Pots.Danger, fontSize = 13.sp, fontWeight = FontWeight.SemiBold)
        }
    }
}

@Composable
private fun TaskSection(goal: GoalView, bucket: TaskBucket) {
    val tasks = goal.tasks.filter { it.bucket == bucket }
    if (tasks.isEmpty()) return
    val notice = LocalNotice.current
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        SectionLabel("${bucket.label} · ${tasks.size}")
        GroupedList(tasks, radius = 12.dp) { task -> TaskRow(task) { notice("任務 thread「${task.title}」還沒實作") } }
    }
}

@Composable
private fun TaskRow(task: TaskView, onClick: () -> Unit) {
    ListRow(
        title = task.title,
        subtitle = task.detail,
        onClick = onClick,
        leading = {
            when (task.bucket) {
                TaskBucket.Running -> Icon(PotsIcons.Spinner, contentDescription = "進行中", tint = Pots.Accent, modifier = Modifier.size(18.dp))
                TaskBucket.Waiting -> Icon(PotsIcons.Clock, contentDescription = "等待中", tint = Pots.Warn, modifier = Modifier.size(18.dp))
                TaskBucket.Done -> Icon(PotsIcons.Check, contentDescription = "完成", tint = Pots.Success, modifier = Modifier.size(18.dp))
            }
        },
        trailing = { task.elapsed?.let { Text(it, fontSize = 12.sp, color = Pots.Muted) } },
    )
}

@Composable
private fun PrRow(store: PotsStore, pr: PrView, onClick: () -> Unit) {
    ListRow(
        title = "#${pr.number}",
        subtitle = pr.title,
        onClick = onClick,
        minHeight = 52.dp,
        trailing = {
            when (val ci = pr.ci) {
                is CiState.Passed -> Tag("CI ${ci.total}/${ci.total}", Pots.Success, Pots.SuccessSoft)
                is CiState.Running -> Tag("CI ${ci.done}/${ci.total} 執行中", Pots.AccentStrong, Pots.AccentSoft)
            }
            pr.approval?.let { id ->
                when (store.decision(id)) {
                    null -> Tag("待核准", Pots.WarnStrong, Pots.WarnSoft)
                    else -> Tag(store.decision(id)!!.label, Pots.InkSoft, Pots.Neutral)
                }
            }
        },
    )
}

// ---------------------------------------------------------------------------
// 記憶、自動化、電腦

@Composable
private fun MemoryTab(memory: List<Observation>) {
    if (memory.isEmpty()) return EmptyNote("這個 project 還沒有觀察紀錄。")
    SectionLabel("觀察紀錄 · ${memory.size}")
    memory.forEach { obs ->
        PotsCard(radius = 12.dp, contentPadding = androidx.compose.foundation.layout.PaddingValues(12.dp), verticalSpacing = 6.dp) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(obs.source, fontSize = 12.sp, color = Pots.Muted, modifier = Modifier.weight(1f))
                if (obs.pinned) Tag("釘選", Pots.AccentStrong, Pots.AccentSoft, fontSize = 11.sp)
            }
            Text(obs.text, fontSize = 14.sp, lineHeight = 21.sp, color = Pots.Ink)
        }
    }
    Text("編輯、釘選與刪除在桌面客戶端做。", fontSize = 12.sp, color = Pots.Muted)
}

@Composable
private fun AutomationsTab(automations: List<AutomationView>) {
    if (automations.isEmpty()) return EmptyNote("這個 project 沒有排程或 listener。")
    val notice = LocalNotice.current
    // 開關只改本地狀態：HTTP API 還沒定義，和桌面版一樣。
    val enabled = remember { mutableStateMapOf<String, Boolean>() }
    SectionLabel("排程與 Listener")
    GroupedList(automations, radius = 12.dp) { item ->
        val on = enabled[item.name] ?: item.enabled
        ListRow(
            title = item.name,
            subtitle = "${item.kind.label} · ${item.detail}",
            onClick = null,
            trailing = {
                if (item.pendingApproval) {
                    Tag("待核准", Pots.WarnStrong, Pots.WarnSoft)
                } else {
                    Switch(
                        checked = on,
                        onCheckedChange = {
                            enabled[item.name] = it
                            notice(if (it) "已啟用「${item.name}」" else "已停用「${item.name}」")
                        },
                        colors = SwitchDefaults.colors(checkedTrackColor = Pots.Accent, uncheckedTrackColor = Pots.Neutral, uncheckedBorderColor = Pots.Control),
                    )
                }
            },
        )
    }
}

@Composable
private fun ComputersTab(computers: List<ComputerView>) {
    if (computers.isEmpty()) return EmptyNote("這個 project 沒有託管電腦。")
    val notice = LocalNotice.current
    computers.forEach { computer ->
        PotsCard(radius = 12.dp) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                Icon(PotsIcons.Monitor, contentDescription = null, tint = Pots.Ink, modifier = Modifier.size(22.dp))
                Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    Text(computer.name, fontSize = 15.sp, fontWeight = FontWeight.SemiBold, color = Pots.Ink)
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        StatusDot(if (computer.connection == ConnectionState.Connected) Pots.Success else Pots.Idle)
                        Text(computer.connection.label, fontSize = 12.sp, color = Pots.Muted)
                    }
                }
            }
            Text("Lease：${computer.lease}", fontSize = 13.sp, color = Pots.Muted)
            Text("權限：${computer.permissions}", fontSize = 13.sp, color = Pots.Muted)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                PrimaryButton("接手", onClick = { notice("接手畫面還沒實作") }, height = 44.dp, fontSize = 15.sp, modifier = Modifier.weight(1f))
                SecondaryButton("停止", onClick = { notice("停止電腦操作還沒實作") }, height = 44.dp, fontSize = 15.sp, color = Pots.Danger, borderColor = Pots.DangerBorder, modifier = Modifier.weight(1f))
            }
        }
    }
}
