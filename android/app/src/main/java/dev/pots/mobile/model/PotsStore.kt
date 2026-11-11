package dev.pots.mobile.model

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshots.SnapshotStateList
import androidx.lifecycle.ViewModel

/**
 * 客戶端的整體狀態，對應桌面版的 `Store`。
 *
 * 讀取都是 Compose snapshot state，畫面會跟著重組。使用者動作改本地狀態，同時把要送給
 * 伺服器的指令記進 [outbox]；同一筆核准在 Inbox、對話卡片、核准頁任一處決定，其他地方
 * 與徽章一起更新。
 */
class PotsStore : ViewModel() {
    val instances: List<InstanceView> = Mock.instances
    var currentInstance: InstanceId by mutableStateOf(Mock.HomeServer)
        private set

    val projects: List<ProjectView> = Mock.projects
    val handoffs: List<Handoff> = Mock.handoffs

    private val inbox: List<InboxItem> = Mock.inbox
    private val approvals = (Mock.approvals.associateBy { it.id })
    private val guiApprovals = (Mock.guiApprovals.associateBy { it.id })
    private val statuses = Mock.status()

    private val decisions = mutableStateMapOf<ApprovalId, Decision>()
    private val resolved = mutableStateListOf<InboxItemId>()
    private val streams: Map<ProjectId, SnapshotStateList<StreamItem>> =
        Mock.streams().mapValues { (_, items) -> mutableStateListOf<StreamItem>().apply { addAll(items) } }
    private val sendModes = mutableStateMapOf<ProjectId, SendMode>()

    val overseer: SnapshotStateList<OverseerItem> = mutableStateListOf<OverseerItem>().apply { addAll(Mock.overseer) }
    val overseerSuggestions: List<String> = Mock.overseerSuggestions

    /** 還沒有連線層；之後由 WebSocket 送出並清空。 */
    val outbox: SnapshotStateList<ClientCommand> = mutableStateListOf()

    val instance: InstanceView get() = instances.first { it.id == currentInstance }

    fun selectInstance(id: InstanceId) {
        currentInstance = id
    }

    fun project(id: ProjectId): ProjectView = projects.first { it.id == id }

    // -- Inbox ---------------------------------------------------------------

    private fun isPending(item: InboxItem): Boolean =
        item.id !in resolved && (item.approval == null || item.approval !in decisions)

    /** 還沒處理的項目，保持設計稿的順序。 */
    val pendingInbox: List<InboxItem> get() = inbox.filter(::isPending)

    fun pendingCount(kind: InboxKind? = null): Int = pendingInbox.count { kind == null || it.kind == kind }

    fun pendingFor(project: ProjectId): Int = pendingInbox.count { it.project == project }

    /** 專案列表頂端的橫幅文字；沒有待處理時為 `null`。 */
    val inboxBanner: Pair<String, String>?
        get() {
            val pending = pendingInbox
            if (pending.isEmpty()) return null
            val parts = buildList {
                pendingCount(InboxKind.Approval).takeIf { it > 0 }?.let { add("$it 筆待核准") }
                pendingCount(InboxKind.NeedsYou).takeIf { it > 0 }?.let { add("$it 項需要你") }
            }
            return parts.joinToString(" · ") to "最早一筆 ${ageLabel(pending.maxOf { it.ageMinutes })}"
        }

    fun retry(item: InboxItemId) {
        if (item in resolved) return
        resolved += item
        outbox += ClientCommand.Retry(item)
    }

    // -- 核准 ----------------------------------------------------------------

    fun approval(id: ApprovalId): ApprovalView? = approvals[id]

    fun guiApproval(id: ApprovalId): GuiApprovalView? = guiApprovals[id]

    fun decision(id: ApprovalId): Decision? = decisions[id]

    /** 已決定的核准不能再改；重複呼叫回傳 `false`。 */
    fun decide(id: ApprovalId, decision: Decision, note: String? = null): Boolean {
        if (id in decisions) return false
        decisions[id] = decision
        outbox += ClientCommand.Decide(id, decision, note?.trim()?.takeIf { it.isNotEmpty() })
        return true
    }

    // -- 對話 ----------------------------------------------------------------

    fun stream(project: ProjectId): List<StreamItem> = streams[project].orEmpty()

    fun status(project: ProjectId): ProjectStatusView =
        statuses[project] ?: ProjectStatusView(goal = null, memory = emptyList(), automations = emptyList(), computers = emptyList())

    /** agent 正在跑的 project：設計稿裡只有 auth-service 顯示「正在輸入」。 */
    fun isAgentBusy(project: ProjectId): Boolean = project(project).status == ProjectStatus.WaitingCi

    fun sendMode(project: ProjectId): SendMode = sendModes[project] ?: SendMode.Queue

    fun setSendMode(project: ProjectId, mode: SendMode) {
        sendModes[project] = mode
    }

    fun send(project: ProjectId, text: String, time: String) {
        val body = text.trim()
        if (body.isEmpty()) return
        val mode = sendMode(project)
        val status = when {
            !isAgentBusy(project) -> MsgStatus.Sent
            mode == SendMode.Queue -> MsgStatus.Queued
            else -> MsgStatus.Steering
        }
        streams[project]?.add(StreamItem.UserMsg(body, time, status))
        outbox += ClientCommand.SendMessage(project, body, mode)
    }

    fun stop(project: ProjectId) {
        outbox += ClientCommand.StopRun(project)
    }

    // -- 用量 ----------------------------------------------------------------

    private val costReports = Mock.costReports()
    private val goalBudgetTokens = Mock.goalBudgetTokens

    var usageRange: UsageRange by mutableStateOf(UsageRange.Today)
        private set

    /** 之後切換範圍會向伺服器要新的 `CostReport`；現在三個範圍都是假資料。 */
    fun selectUsageRange(range: UsageRange) {
        usageRange = range
    }

    val usage: CostReport get() = costReports.getValue(usageRange)

    /** Goal 的預算上限（token）；`null` 是不限。 */
    fun goalBudgetTokens(goal: GoalId): Long? = goalBudgetTokens[goal]

    // -- 總管 ----------------------------------------------------------------

    fun askOverseer(text: String, time: String) {
        val body = text.trim()
        if (body.isEmpty()) return
        overseer += OverseerItem.UserMsg(body, time)
        outbox += ClientCommand.AskOverseer(body)
    }
}
