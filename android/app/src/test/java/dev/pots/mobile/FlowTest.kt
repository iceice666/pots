package dev.pots.mobile

import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import dev.pots.mobile.model.ClientCommand
import dev.pots.mobile.model.Decision
import dev.pots.mobile.model.Mock
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.model.UsageRange
import dev.pots.mobile.ui.theme.PotsTheme
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35], qualifiers = "w390dp-h844dp-xhdpi")
class FlowTest {
    @get:Rule
    val compose = createComposeRule()

    /** 從專案列表進對話、核准 PR，回到列表後橫幅與徽章跟著少一筆。 */
    @Test
    fun approveFromChannelUpdatesProjectList() {
        val store = PotsStore()
        compose.setContent { PotsTheme { PotsApp(store = store) } }

        compose.onNodeWithText("2 筆待核准 · 1 項需要你").assertExists()
        compose.onNodeWithText("auth-service").performClick()
        compose.onNodeWithText("# auth-service").assertExists()

        compose.onAllNodesWithText("核准").onFirst().performClick()
        compose.onNodeWithText("已核准").assertExists()
        assertEquals(ClientCommand.Decide(Mock.MergePr42, Decision.Approved, null), store.outbox.single())

        compose.onNode(hasContentDescription("返回專案列表")).performClick()
        compose.onNodeWithText("1 筆待核准 · 1 項需要你").assertExists()
    }

    /** 從設定進用量，切到 7 天後出現有預算的 Goal；沒預算的顯示不限。 */
    @Test
    fun usageFromSettings() {
        val store = PotsStore()
        compose.setContent { PotsTheme { PotsApp(store = store, startRoute = Routes.SETTINGS) } }

        compose.onNodeWithText("Token 用量").performClick()
        compose.onNodeWithText("1.4M").assertExists()
        compose.onNodeWithText("預算 不限 · 7 次 run").assertExists()
        compose.onAllNodesWithText("遷移到新版文件主題").assertCountEquals(0)

        compose.onNodeWithText("7 天").performClick()
        assertEquals(UsageRange.Week, store.usageRange)
        compose.onNodeWithText("遷移到新版文件主題").assertExists()
        compose.onNodeWithText("已用 1.1M / 預算 2M token · 9 次 run").assertExists()

        compose.onNode(hasContentDescription("返回")).performClick()
        compose.onNodeWithText("推播方式").assertExists()
    }

    @Test
    fun inboxFilterAndRetry() {
        val store = PotsStore()
        compose.setContent { PotsTheme { PotsApp(store = store, startRoute = Routes.INBOX) } }

        compose.onNodeWithText("需要你 1").performClick()
        compose.onAllNodesWithText("Merge PR #42 到 main").assertCountEquals(0)
        compose.onNodeWithText("再試一次").performClick()
        compose.onNodeWithText("這個分類沒有待處理的項目。").assertExists()
        compose.onNodeWithText("需要你 0").assertExists()
    }
}
