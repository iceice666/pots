package dev.pots.mobile.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.BuildConfig
import dev.pots.mobile.LocalNotice
import dev.pots.mobile.PotsNav
import dev.pots.mobile.model.Mock
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.model.formatTokens
import dev.pots.mobile.model.formatUsd
import dev.pots.mobile.ui.components.GroupedList
import dev.pots.mobile.ui.components.HeaderBar
import dev.pots.mobile.ui.components.IconTile
import dev.pots.mobile.ui.components.ListRow
import dev.pots.mobile.ui.components.SectionLabel
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Pots

private data class SettingRow(val title: String, val subtitle: String?, val onClick: (() -> Unit)?)

/** 設計稿只有分頁入口；先放實例、通知與推播落地頁的預覽。 */
@Composable
fun SettingsScreen(store: PotsStore, nav: PotsNav) {
    val notice = LocalNotice.current
    Column(Modifier.fillMaxSize().background(Pots.Bg)) {
        HeaderBar {
            Text(
                "設定",
                style = MaterialTheme.typography.headlineMedium,
                color = Pots.Ink,
                modifier = Modifier.statusBarsPadding().padding(start = 16.dp, end = 16.dp, top = 20.dp, bottom = 14.dp),
            )
        }
        Column(
            verticalArrangement = Arrangement.spacedBy(8.dp),
            modifier = Modifier.verticalScroll(rememberScrollState()).padding(16.dp),
        ) {
            SectionLabel("實例", Modifier.padding(start = 4.dp))
            GroupedList(store.instances) { instance ->
                ListRow(
                    title = instance.name,
                    subtitle = "${instance.kind.label} · ${instance.connection.label} · 本週 ${instance.weekBudget.label}",
                    onClick = { notice("實例設定還沒實作") },
                    leading = { IconTile(PotsIcons.Server) },
                    trailing = { Icon(PotsIcons.ChevronRight, contentDescription = null, tint = Pots.Muted, modifier = Modifier.size(18.dp)) },
                )
            }

            SectionLabel("用量", Modifier.padding(start = 4.dp, top = 12.dp))
            val usage = store.usage
            Rows(
                listOf(
                    SettingRow(
                        "Token 用量",
                        "${store.usageRange.label} ${formatTokens(usage.totals.tokens)} token · 估計 ${formatUsd(usage.totals.estimatedCostUsd)}",
                        nav::usage,
                    ),
                ),
            )

            SectionLabel("通知", Modifier.padding(start = 4.dp, top = 12.dp))
            Rows(
                listOf(
                    SettingRow("靜音時段", "23:00–08:00，期間的請求合併成一則推播", { notice("靜音時段設定還沒實作") }),
                    SettingRow("推播方式", "原生通知（多設備版才有）", null),
                ),
            )

            SectionLabel("推播落地頁預覽", Modifier.padding(start = 4.dp, top = 12.dp))
            Rows(
                listOf(
                    SettingRow("核准專頁", "pots://approval/${Mock.MergePr42.raw} · Merge PR #42", { nav.approval(Mock.MergePr42) }),
                    SettingRow("託管電腦 GUI 核准", "pots://gui-approval/${Mock.RenewDomain.raw} · 網域續約付款", { nav.guiApproval(Mock.RenewDomain) }),
                ),
            )

            Text(
                "Pots ${BuildConfig.VERSION_NAME} · 假資料，尚未連線",
                fontSize = 12.sp,
                color = Pots.Muted,
                modifier = Modifier.padding(start = 4.dp, top = 16.dp),
            )
        }
    }
}

@Composable
private fun Rows(rows: List<SettingRow>) {
    GroupedList(rows) { row ->
        ListRow(
            title = row.title,
            subtitle = row.subtitle,
            onClick = row.onClick,
            trailing = if (row.onClick != null) {
                { Icon(PotsIcons.ChevronRight, contentDescription = null, tint = Pots.Muted, modifier = Modifier.size(18.dp)) }
            } else {
                null
            },
        )
    }
}
