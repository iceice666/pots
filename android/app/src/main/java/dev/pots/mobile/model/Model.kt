package dev.pots.mobile.model

/**
 * 手機客戶端的 view model。
 *
 * 型別命名對齊 `crates/pots-desktop/src/model.rs`（也就是 `proposal/architecture.md`
 * 與 `proposal/ux-and-frontend.md` 的即時協定），是伺服器推給 UI 的「檢視」。現在由
 * [Mock] 填入假資料；之後接 WebSocket 時，事件直接套用到 [PotsStore]。
 */

@JvmInline value class InstanceId(val raw: Long)
@JvmInline value class ProjectId(val raw: Long)
@JvmInline value class InboxItemId(val raw: Long)
@JvmInline value class ApprovalId(val raw: Long)

/** `$4.20` 這類金額顯示，和桌面版 `format_dollars` 一致。 */
fun formatDollars(cents: Int): String =
    if (cents % 100 == 0) "$${cents / 100}" else "$${cents / 100}.${"%02d".format(cents % 100)}"

/** 「2 分鐘前」這類相對時間。 */
fun ageLabel(minutes: Int): String = when {
    minutes < 1 -> "剛剛"
    minutes < 60 -> "$minutes 分鐘前"
    minutes < 24 * 60 -> "${minutes / 60} 小時前"
    else -> "${minutes / (24 * 60)} 天前"
}

/** UI 用的語意色；實際顏色在 `ui.components.Tone*`。 */
enum class Tone { Accent, Warn, Neutral, Success, Violet }

// ---------------------------------------------------------------------------
// 實例與 project

enum class InstanceKind(val label: String) {
    /** 桌面版：app 內嵌服務，經 loopback 連線。手機通常連不到。 */
    Desktop("桌面版"),

    /** 多設備版：自架的 `pots-server`。 */
    MultiDevice("多設備版"),
}

enum class ConnectionState(val label: String) {
    Connected("已連線"),
    Reconnecting("重新連線中"),
    Offline("離線"),
}

data class InstanceView(
    val id: InstanceId,
    val name: String,
    val kind: InstanceKind,
    val connection: ConnectionState,
    /** 這個實例的週預算合計。 */
    val weekBudget: Budget,
)

data class Budget(val spentCents: Int, val limitCents: Int) {
    val label: String get() = "${formatDollars(spentCents)} / ${formatDollars(limitCents)}"
}

/** 專案列表上的狀態列。 */
enum class ProjectStatus(val label: String, val tone: Tone) {
    Idle("閒置", Tone.Neutral),
    Thinking("思考中", Tone.Accent),
    WaitingCi("等待 CI", Tone.Accent),
    NeedsYou("需要你處理", Tone.Warn),
    GoalDone("Goal 完成", Tone.Success),
}

data class ProjectView(
    val id: ProjectId,
    val name: String,
    val avatarTone: Tone,
    val status: ProjectStatus,
    /** 最後活動時間，已格式化（「11:05」「昨天」）。 */
    val lastActivity: String,
    /** 列表第三行：最新一則的摘要。 */
    val summary: String,
    val weekBudget: Budget,
) {
    val initial: String get() = name.take(1).uppercase()
}

/** 已移交到其他實例的 project，列表底部顯示新位置。 */
data class Handoff(val project: String, val target: String)

// ---------------------------------------------------------------------------
// Inbox 與核准

enum class InboxKind(val label: String) {
    Approval("核准"),
    NeedsYou("需要你處理"),
}

enum class Decision(val label: String) {
    Approved("已核准"),
    Rejected("已拒絕"),
}

data class InboxItem(
    val id: InboxItemId,
    val kind: InboxKind,
    val project: ProjectId,
    val title: String,
    val detail: String,
    val ageMinutes: Int,
    /** `kind == Approval` 時指向核准內容。 */
    val approval: ApprovalId? = null,
    /** 「需要你處理」的主要動作，例如「再試一次」。 */
    val primaryAction: String? = null,
)

/** 核准頁「將被執行的動作」表格的一列。 */
data class ActionField(val label: String, val value: String, val mono: Boolean = false)

data class FileChange(val path: String, val added: Int, val removed: Int)

data class ApprovalView(
    val id: ApprovalId,
    val project: ProjectId,
    val goal: String?,
    val title: String,
    /** 卡片上的一行摘要：CI、行數、檔案數。 */
    val summary: String,
    val expiresInHours: Int,
    val action: List<ActionField>,
    val checks: List<String>,
    val files: List<FileChange>,
    val totalFiles: Int,
    /** 核准按鈕文字，例如「核准 merge」。 */
    val confirmLabel: String = "核准",
) {
    val added: Int get() = files.sumOf { it.added }
    val removed: Int get() = files.sumOf { it.removed }
}

/** 託管電腦上的 GUI 動作核准：附截圖與 agent 要點擊的位置。 */
data class GuiApprovalView(
    val id: ApprovalId,
    val project: ProjectId,
    val computer: String,
    val title: String,
    val subtitle: String,
    val pageUrl: String,
    val lineItem: String,
    val amount: String,
    val target: String,
    val capturedMinutesAgo: Int,
)

// ---------------------------------------------------------------------------
// 對話流

enum class SendMode(val label: String) {
    /** agent 忙碌時排到目前這輪之後。 */
    Queue("排隊"),

    /** 打斷目前這輪，改變方向。 */
    Steer("改方向"),
}

enum class MsgStatus { Sent, Queued, Steering }

sealed interface StreamItem {
    /** 「你不在期間」摘要卡。 */
    data class Away(val duration: String, val text: String) : StreamItem
    data class UserMsg(val text: String, val time: String, val status: MsgStatus = MsgStatus.Sent) : StreamItem
    data class AgentMsg(val text: String) : StreamItem
    data class GoalCard(val goal: GoalView) : StreamItem
    data class PrEvent(val text: String, val time: String) : StreamItem
    data class ApprovalCard(val approval: ApprovalId) : StreamItem

    /** 頭像欄裡的項目（agent 這側），連續的只畫一次頭像。 */
    val isAgentSide: Boolean get() = this is AgentMsg || this is GoalCard || this is PrEvent
}

// ---------------------------------------------------------------------------
// 狀態抽屜

enum class TaskBucket(val label: String) {
    Running("進行中"),
    Waiting("等待中"),
    Done("完成"),
}

data class TaskView(
    val title: String,
    val bucket: TaskBucket,
    /** 「worker · Claude Code · 補錯誤處理的測試」 */
    val detail: String,
    val elapsed: String? = null,
)

sealed interface CiState {
    data class Passed(val total: Int) : CiState
    data class Running(val done: Int, val total: Int) : CiState
}

data class PrView(val number: Int, val title: String, val ci: CiState, val approval: ApprovalId? = null)

data class GoalView(
    val title: String,
    val planner: String,
    val budget: Budget,
    val progress: Float,
    val tasks: List<TaskView>,
    val prs: List<PrView>,
) {
    fun count(bucket: TaskBucket): Int = tasks.count { it.bucket == bucket }
}

data class Observation(val text: String, val source: String, val pinned: Boolean = false)

enum class AutomationKind(val label: String) { Schedule("排程"), Listener("Listener") }

data class AutomationView(
    val name: String,
    val kind: AutomationKind,
    val detail: String,
    val enabled: Boolean,
    val pendingApproval: Boolean = false,
)

data class ComputerView(
    val name: String,
    val connection: ConnectionState,
    val lease: String,
    val permissions: String,
)

/** 一個 project 的狀態抽屜內容。 */
data class ProjectStatusView(
    val goal: GoalView?,
    val memory: List<Observation>,
    val automations: List<AutomationView>,
    val computers: List<ComputerView>,
)

// ---------------------------------------------------------------------------
// 總管

sealed interface OverseerItem {
    data class UserMsg(val text: String, val time: String) : OverseerItem
    data class Reply(val text: String) : OverseerItem
    data class Overview(val rows: List<OverviewRow>) : OverseerItem
}

data class OverviewRow(val project: ProjectId, val kind: InboxKind, val text: String)

// ---------------------------------------------------------------------------
// 送給伺服器的指令

/** 使用者動作要送出的指令，對應桌面版 `ClientCommand`。還沒接連線，只記在 outbox。 */
sealed interface ClientCommand {
    data class Decide(val approval: ApprovalId, val decision: Decision, val note: String?) : ClientCommand
    data class SendMessage(val project: ProjectId, val text: String, val mode: SendMode) : ClientCommand
    data class Retry(val item: InboxItemId) : ClientCommand
    data class StopRun(val project: ProjectId) : ClientCommand
    data class AskOverseer(val text: String) : ClientCommand
}
