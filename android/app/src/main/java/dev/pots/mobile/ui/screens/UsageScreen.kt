package dev.pots.mobile.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.PotsNav
import dev.pots.mobile.model.Billing
import dev.pots.mobile.model.CostReport
import dev.pots.mobile.model.CostTotals
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.model.UsageGroup
import dev.pots.mobile.model.UsageRange
import dev.pots.mobile.model.byGoal
import dev.pots.mobile.model.byModel
import dev.pots.mobile.model.byProject
import dev.pots.mobile.model.formatTokens
import dev.pots.mobile.model.formatUsd
import dev.pots.mobile.ui.components.GroupedList
import dev.pots.mobile.ui.components.HeaderBar
import dev.pots.mobile.ui.components.IconAction
import dev.pots.mobile.ui.components.PotsCard
import dev.pots.mobile.ui.components.SectionLabel
import dev.pots.mobile.ui.components.Segmented
import dev.pots.mobile.ui.components.StatusDot
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Pots

/**
 * 用量：依 `CostReport` 顯示 token 與金額。Goal 預算預設不限、每小時熔斷預設關閉，
 * 先在這裡觀察一陣子再決定預算。
 */
@Composable
fun UsageScreen(store: PotsStore, nav: PotsNav) {
    val report = store.usage
    Column(Modifier.fillMaxSize().background(Pots.Bg)) {
        HeaderBar {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(4.dp),
                modifier = Modifier.statusBarsPadding().padding(start = 8.dp, end = 8.dp, top = 8.dp, bottom = 10.dp),
            ) {
                IconAction(PotsIcons.ChevronLeft, "返回", nav::back)
                Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
                    Text("用量", fontSize = 17.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
                    Text("${store.instance.name} · ${report.since} – ${report.until}", fontSize = 12.sp, color = Pots.Muted)
                }
            }
        }
        Column(
            verticalArrangement = Arrangement.spacedBy(8.dp),
            modifier = Modifier.verticalScroll(rememberScrollState()).padding(16.dp),
        ) {
            Segmented(
                options = UsageRange.entries,
                selected = store.usageRange,
                label = { it.label },
                onSelect = store::selectUsageRange,
                description = "時間範圍",
            )

            TotalsCard(report.totals)
            if (report.totals.unpricedTokens > 0) UnpricedNotice(report)

            SectionLabel("依 project", Modifier.padding(start = 4.dp, top = 12.dp))
            Groups(report.byProject()) { "${it.runs} 次 run · ${billingLabel(it.billing)}" }

            SectionLabel("依模型", Modifier.padding(start = 4.dp, top = 12.dp))
            Groups(report.byModel()) { "${billingLabel(it.billing)} · ${it.runs} 次 run" }

            SectionLabel("依 Goal", Modifier.padding(start = 4.dp, top = 12.dp))
            val goals = report.byGoal()
            if (goals.isEmpty()) {
                Text("這段時間沒有 Goal 的 run。", fontSize = 13.sp, color = Pots.Muted, modifier = Modifier.padding(start = 4.dp))
            } else {
                Groups(goals) { group ->
                    val budget = group.goal?.let(store::goalBudgetTokens)
                    val label = if (budget == null) "預算 不限" else "已用 ${formatTokens(group.tokens)} / 預算 ${formatTokens(budget)} token"
                    "$label · ${group.runs} 次 run"
                }
            }

            Text(
                "產生於 ${report.generatedAt} · 金額是估計，不是帳單",
                fontSize = 12.sp,
                color = Pots.Muted,
                modifier = Modifier.padding(start = 4.dp, top = 16.dp),
            )
        }
    }
}

private fun billingLabel(billing: List<Billing>): String = billing.joinToString("／") { it.label }

@Composable
private fun TotalsCard(totals: CostTotals) {
    PotsCard(verticalSpacing = 14.dp) {
        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text("總 token", fontSize = 12.sp, fontWeight = FontWeight.Bold, color = Pots.Muted)
            Text(formatTokens(totals.tokens), fontSize = 28.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
            Text(
                "input ${formatTokens(totals.inputTokens)} · output ${formatTokens(totals.outputTokens)} · ${totals.runs} 次 run",
                fontSize = 13.sp,
                color = Pots.InkSoft,
            )
        }
        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Stat("估計金額", formatUsd(totals.estimatedCostUsd), "依單價表", Modifier.weight(1f))
            Stat("CLI 回報", formatUsd(totals.reportedCostUsd), "runner 自己算的", Modifier.weight(1f))
        }
        HorizontalDivider(color = Pots.Divider)
        Split(
            "計費方式",
            listOf(
                Part(Billing.Subscription.label, totals.subscriptionTokens, Pots.Accent),
                Part(Billing.Api.label, totals.apiTokens, Pots.Amber),
                Part(Billing.Cli.label, totals.cliTokens, Pots.Violet),
                Part(Billing.Unknown.label, totals.tokens - totals.subscriptionTokens - totals.apiTokens - totals.cliTokens, Pots.Idle),
            ),
        )
        Split(
            "用途",
            listOf(
                Part("主要工作", totals.workTokens, Pots.Accent),
                Part("記憶整理（OM）", totals.omTokens, Pots.Success),
            ),
        )
    }
}

@Composable
private fun Stat(label: String, value: String, hint: String, modifier: Modifier = Modifier) {
    Column(
        verticalArrangement = Arrangement.spacedBy(2.dp),
        modifier = modifier
            .background(Pots.SurfaceSunken, RoundedCornerShape(10.dp))
            .border(1.dp, Pots.Border, RoundedCornerShape(10.dp))
            .padding(horizontal = 12.dp, vertical = 10.dp),
    ) {
        Text(label, fontSize = 12.sp, color = Pots.Muted)
        Text(value, fontSize = 18.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
        Text(hint, fontSize = 11.sp, color = Pots.Muted)
    }
}

private data class Part(val label: String, val tokens: Long, val color: Color)

/** 比例條加圖例；0 token 的部分不畫。 */
@Composable
private fun Split(title: String, parts: List<Part>) {
    val shown = parts.filter { it.tokens > 0 }
    val total = shown.sumOf { it.tokens }.coerceAtLeast(1)
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Text(title, fontSize = 12.sp, fontWeight = FontWeight.Bold, color = Pots.Muted)
        Row(
            Modifier
                .fillMaxWidth()
                .height(8.dp)
                .clip(RoundedCornerShape(4.dp))
                .background(Pots.Neutral),
        ) {
            shown.forEach { part -> Box(Modifier.weight(part.tokens.toFloat()).fillMaxHeight().background(part.color)) }
        }
        shown.forEach { part ->
            val pct = (part.tokens * 100 / total).toInt()
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                modifier = Modifier.semantics(mergeDescendants = true) {},
            ) {
                StatusDot(part.color, size = 8.dp)
                Text(part.label, fontSize = 13.sp, color = Pots.Ink, modifier = Modifier.weight(1f))
                Text("${formatTokens(part.tokens)} · $pct%", fontSize = 13.sp, color = Pots.InkSoft)
            }
        }
    }
}

@Composable
private fun UnpricedNotice(report: CostReport) {
    val pricing = report.pricing
    val source = if (pricing.source.isEmpty()) {
        "還沒有任何單價"
    } else {
        "單價表：${pricing.source} · ${pricing.models} 個模型" + (pricing.updatedAt?.let { " · $it 更新" } ?: "")
    }
    Row(
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        modifier = Modifier
            .fillMaxWidth()
            .background(Pots.WarnSoft, RoundedCornerShape(12.dp))
            .border(1.dp, Pots.WarnBorder, RoundedCornerShape(12.dp))
            .padding(12.dp),
    ) {
        Icon(PotsIcons.Warning, contentDescription = null, tint = Pots.Warn, modifier = Modifier.size(18.dp))
        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(
                "${formatTokens(report.totals.unpricedTokens)} token 沒有單價，不在估計金額內",
                fontSize = 14.sp,
                fontWeight = FontWeight.SemiBold,
                color = Pots.WarnStrong,
            )
            Text("訂閱與 CLI 用量不換算；API 模型不在單價表裡也算不出。$source", fontSize = 12.sp, color = Pots.WarnStrong, lineHeight = 17.sp)
        }
    }
}

@Composable
private fun Groups(groups: List<UsageGroup>, subtitle: (UsageGroup) -> String) {
    GroupedList(groups) { group ->
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 12.dp, vertical = 10.dp)
                .semantics(mergeDescendants = true) {},
        ) {
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(group.title, fontSize = 15.sp, fontWeight = FontWeight.SemiBold, color = Pots.Ink)
                Text(subtitle(group), fontSize = 12.sp, color = Pots.Muted)
            }
            Column(horizontalAlignment = Alignment.End, verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(formatTokens(group.tokens), fontSize = 15.sp, fontWeight = FontWeight.SemiBold, color = Pots.Ink)
                Text(costLabel(group), fontSize = 12.sp, color = Pots.Muted, textAlign = TextAlign.End)
            }
        }
    }
}

/** 列右下角：有估計就顯示估計，有 CLI 回報另外標出；兩者都沒有是訂閱或沒有單價。 */
private fun costLabel(group: UsageGroup): String = buildList {
    if (group.estimatedCostUsd > 0) add("估計 ${formatUsd(group.estimatedCostUsd)}")
    if (group.reportedCostUsd > 0) add("CLI ${formatUsd(group.reportedCostUsd)}")
}.joinToString(" · ").ifEmpty { "沒有金額" }
