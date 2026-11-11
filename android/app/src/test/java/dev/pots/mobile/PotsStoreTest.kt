package dev.pots.mobile

import dev.pots.mobile.model.ClientCommand
import dev.pots.mobile.model.Decision
import dev.pots.mobile.model.InboxKind
import dev.pots.mobile.model.Mock
import dev.pots.mobile.model.MsgStatus
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.model.SendMode
import dev.pots.mobile.model.StreamItem
import dev.pots.mobile.model.UsageRange
import dev.pots.mobile.model.byGoal
import dev.pots.mobile.model.byModel
import dev.pots.mobile.model.byProject
import dev.pots.mobile.model.formatTokens
import dev.pots.mobile.model.formatUsd
import dev.pots.mobile.model.ageLabel
import dev.pots.mobile.model.formatDollars
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PotsStoreTest {
    @Test
    fun mockMatchesDesign() {
        val store = PotsStore()
        assertEquals(3, store.pendingCount())
        assertEquals(2, store.pendingCount(InboxKind.Approval))
        assertEquals(1, store.pendingCount(InboxKind.NeedsYou))
        assertEquals("2 筆待核准 · 1 項需要你" to "最早一筆 3 小時前", store.inboxBanner)
        assertEquals(listOf(1, 1, 1, 0), store.projects.map { store.pendingFor(it.id) })
    }

    @Test
    fun decidingUpdatesEveryCount() {
        val store = PotsStore()
        assertTrue(store.decide(Mock.MergePr42, Decision.Approved, "  "))
        assertEquals(Decision.Approved, store.decision(Mock.MergePr42))
        assertEquals(0, store.pendingFor(Mock.AuthService))
        assertEquals(2, store.pendingCount())
        assertEquals("1 筆待核准 · 1 項需要你" to "最早一筆 3 小時前", store.inboxBanner)
        // 空白理由不送出。
        assertEquals(listOf(ClientCommand.Decide(Mock.MergePr42, Decision.Approved, null)), store.outbox)
    }

    @Test
    fun decisionIsFinal() {
        val store = PotsStore()
        assertTrue(store.decide(Mock.InfraListener, Decision.Rejected, "先不要"))
        assertFalse(store.decide(Mock.InfraListener, Decision.Approved))
        assertEquals(Decision.Rejected, store.decision(Mock.InfraListener))
        assertEquals(1, store.outbox.size)
    }

    @Test
    fun clearingEverythingHidesBanner() {
        val store = PotsStore()
        store.decide(Mock.MergePr42, Decision.Approved)
        store.decide(Mock.InfraListener, Decision.Approved)
        store.retry(store.pendingInbox.single().id)
        assertEquals(0, store.pendingCount())
        assertNull(store.inboxBanner)
    }

    @Test
    fun sendingWhileBusyFollowsMode() {
        val store = PotsStore()
        store.send(Mock.AuthService, "  先等 CI  ", "11:20")
        store.setSendMode(Mock.AuthService, SendMode.Steer)
        store.send(Mock.AuthService, "改成先修前端", "11:21")
        store.send(Mock.DocsSite, "謝謝", "11:22")
        store.send(Mock.DocsSite, "   ", "11:23")

        val auth = store.stream(Mock.AuthService).filterIsInstance<StreamItem.UserMsg>().takeLast(2)
        assertEquals(listOf("先等 CI" to MsgStatus.Queued, "改成先修前端" to MsgStatus.Steering), auth.map { it.text to it.status })
        assertEquals(MsgStatus.Sent, (store.stream(Mock.DocsSite).last() as StreamItem.UserMsg).status)
        assertEquals(3, store.outbox.size)
    }

    @Test
    fun usageReportAddsUp() {
        val store = PotsStore()
        assertEquals(UsageRange.Today, store.usageRange)
        val today = store.usage
        assertEquals(1_352_400L, today.totals.tokens)
        assertEquals(today.rows.sumOf { it.tokens }, today.totals.tokens)
        assertEquals(171_500L, today.totals.pricedTokens)
        assertEquals(today.totals.tokens, today.totals.pricedTokens + today.totals.unpricedTokens)
        assertEquals(74_300L, today.totals.omTokens)
        assertEquals(today.totals.tokens, today.totals.omTokens + today.totals.workTokens)
        // 彙總不會漏掉或重算任何一列。
        assertEquals(today.totals.tokens, today.byProject().sumOf { it.tokens })
        assertEquals(today.totals.tokens, today.byModel().sumOf { it.tokens })
        assertEquals("auth-service", today.byProject().first().title)

        val oauth = today.byGoal().first()
        assertEquals("OAuth 登入遷移" to 656_300L, oauth.title to oauth.tokens)
        assertNull(store.goalBudgetTokens(oauth.goal!!))

        store.selectUsageRange(UsageRange.Month)
        val month = store.usage
        assertTrue(month.totals.tokens > today.totals.tokens)
        // 舊的 run 沒有 billing，不算進任何一種帳。
        assertTrue(month.totals.subscriptionTokens + month.totals.apiTokens + month.totals.cliTokens < month.totals.tokens)
        assertEquals(2_000_000L, store.goalBudgetTokens(Mock.DocsThemeGoal))
    }

    @Test
    fun formatting() {
        assertEquals("850", formatTokens(850))
        assertEquals("12.4K", formatTokens(12_400))
        assertEquals("1.4M", formatTokens(1_352_400))
        assertEquals("2M", formatTokens(2_000_000))
        assertEquals("$1.92", formatUsd(1.92))
        assertEquals("<$0.01", formatUsd(0.004))
        assertEquals("$0", formatUsd(0.0))
        assertEquals("$4.20", formatDollars(420))
        assertEquals("$20", formatDollars(2000))
        assertEquals("2 分鐘前", ageLabel(2))
        assertEquals("3 小時前", ageLabel(180))
        assertEquals("剛剛", ageLabel(0))
    }
}
