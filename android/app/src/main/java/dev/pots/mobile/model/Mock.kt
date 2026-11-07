package dev.pots.mobile.model

/** 假資料。專案、Inbox、核准與總管的內容取自手機設計稿。 */
object Mock {
    val HomeServer = InstanceId(1)

    val AuthService = ProjectId(1)
    val WebApp = ProjectId(2)
    val Infra = ProjectId(3)
    val DocsSite = ProjectId(4)

    val MergePr42 = ApprovalId(1)
    val InfraListener = ApprovalId(2)
    val RenewDomain = ApprovalId(3)

    val instances = listOf(
        InstanceView(HomeServer, "家用伺服器", InstanceKind.MultiDevice, ConnectionState.Connected, Budget(780, 5000)),
    )

    val projects = listOf(
        ProjectView(AuthService, "auth-service", Tone.Accent, ProjectStatus.WaitingCi, "11:05", "需要你核准：Merge PR #42 到 main", Budget(420, 2000)),
        ProjectView(WebApp, "web-app", Tone.Warn, ProjectStatus.NeedsYou, "08:12", "CI 連續失敗 3 次，已停止自動修復", Budget(210, 1500)),
        ProjectView(Infra, "infra", Tone.Violet, ProjectStatus.Idle, "10:04", "新的 listener：main CI 失敗時自動修復", Budget(60, 1000)),
        ProjectView(DocsSite, "docs-site", Tone.Neutral, ProjectStatus.GoalDone, "昨天", "遷移到新版文件主題，已部署", Budget(90, 500)),
    )

    val handoffs = listOf(Handoff("legacy-api", "我的筆電"))

    val inbox = listOf(
        InboxItem(InboxItemId(1), InboxKind.Approval, AuthService, "Merge PR #42 到 main", "後端 OAuth callback · CI 12 / 12 通過 · +182 −40", 2, MergePr42),
        InboxItem(InboxItemId(2), InboxKind.NeedsYou, WebApp, "CI 連續失敗 3 次，已停止自動修復", "可能是不穩定的測試：e2e/checkout.spec.ts", 180, primaryAction = "再試一次"),
        InboxItem(InboxItemId(3), InboxKind.Approval, Infra, "新的 listener：main CI 失敗時自動修復", "每次觸發上限 $1 · 每天最多 5 次", 60, InfraListener),
    )

    val approvals = listOf(
        ApprovalView(
            id = MergePr42,
            project = AuthService,
            goal = "OAuth 登入遷移",
            title = "Merge PR #42 到 main",
            summary = "CI 12 / 12 通過 · +182 −40 · 6 個檔案",
            expiresInHours = 71,
            action = listOf(
                ActionField("動作", "MergePr", mono = true),
                ActionField("PR", "#42 oauth/callback → main"),
                ActionField("方式", "squash"),
                ActionField("雜湊", "a3f9 c2e1 7b04", mono = true),
            ),
            checks = listOf("CI 12 / 12 通過", "reviewer subagent 沒有阻擋意見"),
            files = listOf(
                FileChange("src/oauth/callback.rs", 96, 0),
                FileChange("src/session.rs", 41, 28),
                FileChange("src/oauth/provider.rs", 24, 6),
                FileChange("src/routes.rs", 9, 4),
                FileChange("tests/oauth_callback.rs", 10, 0),
                FileChange("Cargo.toml", 2, 2),
            ),
            totalFiles = 6,
            confirmLabel = "核准 merge",
        ),
        ApprovalView(
            id = InfraListener,
            project = Infra,
            goal = null,
            title = "新的 listener：main CI 失敗時自動修復",
            summary = "每次觸發上限 $1 · 每天最多 5 次",
            expiresInHours = 47,
            action = listOf(
                ActionField("動作", "CreateListener", mono = true),
                ActionField("條件", "main 分支 CI 失敗"),
                ActionField("動作內容", "開修復任務"),
                ActionField("上限", "每次 $1 · 每天 5 次"),
            ),
            checks = listOf("條件只比對 main 分支", "超過上限會改成通知你"),
            files = emptyList(),
            totalFiles = 0,
            confirmLabel = "核准啟用",
        ),
    )

    val guiApprovals = listOf(
        GuiApprovalView(
            id = RenewDomain,
            project = Infra,
            computer = "Mac mini（客廳）",
            title = "在網域註冊商後台送出續約付款",
            subtitle = "pots.dev 續約 1 年 · [金額]",
            pageUrl = "registrar.example/renew",
            lineItem = "pots.dev · 1 年",
            amount = "[金額]",
            target = "確認付款",
            capturedMinutesAgo = 2,
        ),
    )

    private val oauthGoal = GoalView(
        title = "OAuth 登入遷移",
        planner = "Planner · 4 個任務",
        budget = Budget(152, 400),
        progress = 0.38f,
        tasks = listOf(
            TaskView("後端 OAuth callback", TaskBucket.Running, "worker · Claude Code · 補錯誤處理的測試", "12 分"),
            TaskView("前端登入按鈕", TaskBucket.Running, "worker · rig · 修改登入頁", "8 分"),
            TaskView("整合測試", TaskBucket.Waiting, "等待 CI · PR #44"),
            TaskView("Spec 審查", TaskBucket.Done, "Planner"),
        ),
        prs = listOf(
            PrView(42, "後端 OAuth callback", CiState.Passed(12), MergePr42),
            PrView(44, "整合測試", CiState.Running(7, 12)),
        ),
    )

    fun streams(): Map<ProjectId, List<StreamItem>> = mapOf(
        AuthService to listOf(
            StreamItem.Away("3 小時", "Goal「OAuth 登入遷移」開出 PR #42，CI 全數通過，等你核准 merge。前端按鈕仍在進行。"),
            StreamItem.UserMsg("session 過期那段我改了，其他沒問題，可以開工。", "10:21"),
            StreamItem.AgentMsg("好，已照你改過的 spec 開了 Goal，Planner 拆成 4 個任務。"),
            StreamItem.GoalCard(oauthGoal),
            StreamItem.PrEvent("PR #42 已開：後端 OAuth callback", "10:48"),
            StreamItem.ApprovalCard(MergePr42),
        ),
        WebApp to listOf(
            StreamItem.AgentMsg("main 上的 CI 連續失敗 3 次，都卡在 e2e/checkout.spec.ts。看起來是不穩定的測試，我先停止自動修復，等你決定。"),
        ),
        Infra to listOf(
            StreamItem.AgentMsg("我提了一個新的 listener：main CI 失敗時自動開修復任務，每次上限 $1、每天最多 5 次。核准後才會啟用。"),
            StreamItem.ApprovalCard(InfraListener),
        ),
        DocsSite to listOf(
            StreamItem.AgentMsg("Goal「遷移到新版文件主題」完成，已部署。"),
        ),
    )

    fun status(): Map<ProjectId, ProjectStatusView> = mapOf(
        AuthService to ProjectStatusView(
            goal = oauthGoal,
            memory = listOf(
                Observation("session 過期時間改為 30 分鐘，refresh token 7 天", "你在 spec 裡的修改", pinned = true),
                Observation("整合測試要先起 Redis，CI 用 services 區塊", "worker 觀察"),
            ),
            automations = listOf(
                AutomationView("PR 有新的 review 留言時回覆", AutomationKind.Listener, "這個 project · 每天最多 10 次", enabled = true),
                AutomationView("每週一 09:00 檢查依賴更新", AutomationKind.Schedule, "對話 agent 建立", enabled = true),
                AutomationView("main CI 失敗時自動修復", AutomationKind.Listener, "所有專案 · 待核准", enabled = false, pendingApproval = true),
            ),
            computers = listOf(
                ComputerView("Mac mini（客廳）", ConnectionState.Connected, "沒有人持有", "螢幕錄製、輔助使用已授權"),
            ),
        ),
    )

    val overseer = listOf(
        OverseerItem.UserMsg("今天各專案卡在哪？", "11:12"),
        OverseerItem.Reply("三個專案在等你，其中兩個只要一鍵："),
        OverseerItem.Overview(
            listOf(
                OverviewRow(AuthService, InboxKind.Approval, "PR #42 CI 全過，等 merge"),
                OverviewRow(WebApp, InboxKind.NeedsYou, "e2e/checkout 不穩定，建議先 skip 再開 issue"),
                OverviewRow(Infra, InboxKind.Approval, "新 listener，每天上限 5 次"),
            ),
        ),
        OverseerItem.Reply("docs-site 昨天已完成，本週預算合計用了 $7.80 / $50。"),
    )

    val overseerSuggestions = listOf("幫 web-app 開 Goal", "本週成本報表")
}
