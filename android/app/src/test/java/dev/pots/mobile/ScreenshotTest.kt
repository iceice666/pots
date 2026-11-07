package dev.pots.mobile

import androidx.compose.runtime.Composable
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import androidx.navigation.compose.rememberNavController
import com.github.takahirom.roborazzi.ExperimentalRoborazziApi
import com.github.takahirom.roborazzi.captureRoboImage
import com.github.takahirom.roborazzi.captureScreenRoboImage
import dev.pots.mobile.model.Mock
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.ui.screens.ApprovalScreen
import dev.pots.mobile.ui.screens.ChannelScreen
import dev.pots.mobile.ui.screens.GuiApprovalScreen
import dev.pots.mobile.ui.theme.PotsTheme
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * 用 Robolectric 的 native graphics 輸出主要畫面的 PNG，尺寸對齊設計稿的 390×844。
 *
 * `./gradlew :app:recordRoborazziDebug` 會寫到 `app/build/outputs/roborazzi/`。
 * 這只是產生圖檔方便和設計稿對照，不是 golden-image 比對。
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [35], qualifiers = "w390dp-h844dp-xhdpi")
class ScreenshotTest {
    @get:Rule
    val compose = createComposeRule()

    private fun app(route: String, name: String) = shot(name) { PotsApp(store = PotsStore(), startRoute = route) }

    private fun shot(name: String, content: @Composable () -> Unit) {
        compose.setContent { PotsTheme(content) }
        compose.waitForIdle()
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/$name.png")
    }

    @Composable
    private fun nav() = PotsNav(rememberNavController())

    @Test fun projects() = app(Routes.PROJECTS, "01-projects")

    @Test fun channel() = shot("02-channel") { ChannelScreen(PotsStore(), nav(), Mock.AuthService) }

    @OptIn(ExperimentalRoborazziApi::class)
    @Test
    fun statusDrawer() {
        compose.setContent { PotsTheme { ChannelScreen(PotsStore(), nav(), Mock.AuthService, initiallyShowStatus = true) } }
        compose.waitForIdle()
        captureScreenRoboImage("build/outputs/roborazzi/03-status-drawer.png")
    }

    @Test fun inbox() = app(Routes.INBOX, "04-inbox")

    @Test fun approval() = shot("05-approval") { ApprovalScreen(PotsStore(), nav(), Mock.MergePr42) }

    @Test fun guiApproval() = shot("06-gui-approval") { GuiApprovalScreen(PotsStore(), nav(), Mock.RenewDomain) }

    @Test fun overseer() = app(Routes.OVERSEER, "07-overseer")

    @Test fun settings() = app(Routes.SETTINGS, "08-settings")
}
