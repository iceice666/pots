package dev.pots.mobile

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.NavHostController
import androidx.navigation.NavType
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.currentBackStackEntryAsState
import androidx.navigation.compose.rememberNavController
import androidx.navigation.navArgument
import androidx.navigation.navDeepLink
import dev.pots.mobile.model.ApprovalId
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.model.ProjectId
import dev.pots.mobile.ui.components.CountBadge
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.screens.ApprovalScreen
import dev.pots.mobile.ui.screens.ChannelScreen
import dev.pots.mobile.ui.screens.GuiApprovalScreen
import dev.pots.mobile.ui.screens.InboxScreen
import dev.pots.mobile.ui.screens.OverseerScreen
import dev.pots.mobile.ui.screens.ProjectsScreen
import dev.pots.mobile.ui.screens.SettingsScreen
import dev.pots.mobile.ui.theme.Pots
import kotlinx.coroutines.launch

object Routes {
    const val PROJECTS = "projects"
    const val INBOX = "inbox"
    const val OVERSEER = "overseer"
    const val SETTINGS = "settings"
    const val CHANNEL = "channel/{project}"
    const val APPROVAL = "approval/{approval}"
    const val GUI_APPROVAL = "gui-approval/{approval}"

    fun channel(id: ProjectId) = "channel/${id.raw}"
    fun approval(id: ApprovalId) = "approval/${id.raw}"
    fun guiApproval(id: ApprovalId) = "gui-approval/${id.raw}"

    val tabs = listOf(PROJECTS, INBOX, OVERSEER, SETTINGS)
}

/** 底部提示；還沒實作的按鈕也用它說明，和桌面版的底部提示一致。 */
val LocalNotice = staticCompositionLocalOf<(String) -> Unit> { {} }

/** 各畫面共用的導航動作。 */
class PotsNav(private val controller: NavHostController) {
    fun tab(route: String) = controller.navigate(route) {
        popUpTo(Routes.PROJECTS) { saveState = true }
        launchSingleTop = true
        restoreState = true
    }

    fun channel(id: ProjectId) = controller.navigate(Routes.channel(id)) { launchSingleTop = true }
    fun approval(id: ApprovalId) = controller.navigate(Routes.approval(id)) { launchSingleTop = true }
    fun guiApproval(id: ApprovalId) = controller.navigate(Routes.guiApproval(id)) { launchSingleTop = true }

    fun back() {
        if (!controller.popBackStack()) tab(Routes.PROJECTS)
    }
}

@Composable
fun PotsApp(
    store: PotsStore = viewModel(),
    startRoute: String = Routes.PROJECTS,
    controller: NavHostController = rememberNavController(),
) {
    val nav = remember(controller) { PotsNav(controller) }
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    val notice: (String) -> Unit = remember(snackbar, scope) {
        { text ->
            scope.launch {
                snackbar.currentSnackbarData?.dismiss()
                snackbar.showSnackbar(text)
            }
        }
    }
    val entry by controller.currentBackStackEntryAsState()
    val route = entry?.destination?.route ?: startRoute

    CompositionLocalProvider(LocalNotice provides notice) {
        Scaffold(
            containerColor = Pots.Bg,
            contentWindowInsets = WindowInsets(0),
            snackbarHost = { SnackbarHost(snackbar) },
            bottomBar = {
                if (route in Routes.tabs) {
                    PotsBottomBar(current = route, inboxCount = store.pendingCount(), onSelect = nav::tab)
                }
            },
        ) { padding ->
            NavHost(
                navController = controller,
                startDestination = startRoute,
                modifier = Modifier.fillMaxSize().padding(padding),
            ) {
                composable(Routes.PROJECTS) { ProjectsScreen(store, nav) }
                composable(Routes.INBOX) { InboxScreen(store, nav) }
                composable(Routes.OVERSEER) { OverseerScreen(store, nav) }
                composable(Routes.SETTINGS) { SettingsScreen(store, nav) }
                composable(
                    Routes.CHANNEL,
                    arguments = listOf(navArgument("project") { type = NavType.LongType }),
                ) { backStack ->
                    ChannelScreen(store, nav, ProjectId(backStack.arguments!!.getLong("project")))
                }
                composable(
                    Routes.APPROVAL,
                    arguments = listOf(navArgument("approval") { type = NavType.LongType }),
                    deepLinks = listOf(navDeepLink { uriPattern = "pots://approval/{approval}" }),
                ) { backStack ->
                    ApprovalScreen(store, nav, ApprovalId(backStack.arguments!!.getLong("approval")))
                }
                composable(
                    Routes.GUI_APPROVAL,
                    arguments = listOf(navArgument("approval") { type = NavType.LongType }),
                    deepLinks = listOf(navDeepLink { uriPattern = "pots://gui-approval/{approval}" }),
                ) { backStack ->
                    GuiApprovalScreen(store, nav, ApprovalId(backStack.arguments!!.getLong("approval")))
                }
            }
        }
    }
}

private data class Tab(val route: String, val label: String, val icon: ImageVector)

private val tabs = listOf(
    Tab(Routes.PROJECTS, "專案", PotsIcons.Hash),
    Tab(Routes.INBOX, "Inbox", PotsIcons.Inbox),
    Tab(Routes.OVERSEER, "總管", PotsIcons.Compass),
    Tab(Routes.SETTINGS, "設定", PotsIcons.Settings),
)

@Composable
private fun PotsBottomBar(current: String, inboxCount: Int, onSelect: (String) -> Unit) {
    Surface(color = Pots.Surface) {
        Column(Modifier.navigationBarsPadding()) {
            HorizontalDivider(color = Pots.Border, thickness = 1.dp)
            Row(Modifier.fillMaxWidth().padding(horizontal = 4.dp, vertical = 6.dp)) {
                tabs.forEach { tab ->
                    val on = tab.route == current
                    val color = if (on) Pots.Accent else Pots.Muted
                    val badge = if (tab.route == Routes.INBOX && inboxCount > 0) inboxCount else null
                    Column(
                        horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.spacedBy(4.dp, Alignment.CenterVertically),
                        modifier = Modifier
                            .weight(1f)
                            .heightIn(min = 52.dp)
                            .clickable(role = Role.Tab) { onSelect(tab.route) }
                            .semantics {
                                selected = on
                                if (badge != null) contentDescription = "${tab.label}，$badge 筆待處理"
                            },
                    ) {
                        Box {
                            Icon(tab.icon, contentDescription = null, tint = color, modifier = Modifier.size(22.dp))
                            if (badge != null) {
                                CountBadge(badge, small = true, modifier = Modifier.align(Alignment.TopStart).offset(x = 14.dp, y = (-6).dp))
                            }
                        }
                        Text(
                            tab.label,
                            color = color,
                            fontSize = 12.sp,
                            fontWeight = if (on) FontWeight.Bold else FontWeight.Normal,
                        )
                    }
                }
            }
        }
    }
}
