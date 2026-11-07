package dev.pots.mobile.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.LocalNotice
import dev.pots.mobile.PotsNav
import dev.pots.mobile.model.ApprovalId
import dev.pots.mobile.model.GuiApprovalView
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.model.ageLabel
import dev.pots.mobile.ui.components.ApprovalButtons
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Pots

/** 託管電腦上的 GUI 動作核准：截圖、要點擊的位置、接手畫面。 */
@Composable
fun GuiApprovalScreen(store: PotsStore, nav: PotsNav, id: ApprovalId) {
    val notice = LocalNotice.current
    val request = store.guiApproval(id) ?: return MissingApproval(nav)
    val project = store.project(request.project)

    Column(Modifier.fillMaxSize().background(Pots.Surface)) {
        ApprovalHeader(chip = "託管電腦", chipFg = Pots.Violet, chipBg = Pots.VioletSoft, onClose = nav::back)

        Column(
            verticalArrangement = Arrangement.spacedBy(14.dp),
            modifier = Modifier.weight(1f).verticalScroll(rememberScrollState()).padding(16.dp),
        ) {
            Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Row {
                    Text("# ${project.name}", fontSize = 13.sp, fontWeight = FontWeight.SemiBold, color = Pots.Ink)
                    Text(" · ${request.computer}", fontSize = 13.sp, color = Pots.Muted)
                }
                Text(request.title, fontSize = 22.sp, fontWeight = FontWeight.Bold, lineHeight = 30.sp, color = Pots.Ink)
                Text(request.subtitle, fontSize = 14.sp, color = Pots.Muted)
            }

            Screenshot(request)

            Row(
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                modifier = Modifier
                    .fillMaxWidth()
                    .background(Pots.WarnSoft, RoundedCornerShape(12.dp))
                    .border(1.dp, Pots.WarnBorder, RoundedCornerShape(12.dp))
                    .padding(12.dp),
            ) {
                Icon(PotsIcons.Shield, contentDescription = null, tint = Pots.WarnStrong, modifier = Modifier.size(18.dp))
                Text("點擊前會再比對一次畫面；和這張截圖不一致就不送出，改請你處理。", fontSize = 13.sp, color = Pots.WarnStrong, lineHeight = 20.sp)
            }

            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(12.dp),
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = 52.dp)
                    .clip(RoundedCornerShape(12.dp))
                    .border(1.dp, Pots.Border, RoundedCornerShape(12.dp))
                    .clickable(role = Role.Button) { notice("接手畫面還沒實作") }
                    .padding(horizontal = 14.dp, vertical = 8.dp),
            ) {
                Icon(PotsIcons.Monitor, contentDescription = null, tint = Pots.Ink, modifier = Modifier.size(20.dp))
                Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    Text("接手畫面", fontSize = 15.sp, fontWeight = FontWeight.SemiBold, color = Pots.Ink)
                    Text("agent 暫停，由你自己操作，完成後交還", fontSize = 12.sp, color = Pots.Muted)
                }
                Icon(PotsIcons.ChevronRight, contentDescription = null, tint = Pots.Muted, modifier = Modifier.size(18.dp))
            }
        }

        DecisionBar {
            ApprovalButtons(store, id, height = 54.dp, approveLabel = "核准送出", reversed = true)
        }
    }
}

/** 截圖的示意：瀏覽器外框、續約明細與被標記的按鈕。之後換成伺服器給的圖。 */
@Composable
private fun Screenshot(request: GuiApprovalView) {
    val frame = RoundedCornerShape(12.dp)
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Column(
            Modifier
                .fillMaxWidth()
                .clip(frame)
                .border(1.dp, Pots.Control, frame)
                .background(Pots.SurfaceSunken)
                .semantics {
                    contentDescription = "${request.computer} 目前畫面的截圖：續約付款頁，${request.target}按鈕被標記為 agent 要點擊的位置"
                },
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
                modifier = Modifier.fillMaxWidth().background(Pots.Neutral).padding(horizontal = 10.dp, vertical = 8.dp),
            ) {
                repeat(3) { Box(Modifier.size(8.dp).background(Pots.InputBorder, CircleShape)) }
                Spacer(Modifier.size(2.dp))
                Text(
                    request.pageUrl,
                    fontSize = 10.sp,
                    lineHeight = 14.sp,
                    maxLines = 1,
                    color = Pots.Muted,
                    modifier = Modifier.weight(1f).background(Pots.Surface, RoundedCornerShape(5.dp)).padding(horizontal = 8.dp, vertical = 2.dp),
                )
            }
            HorizontalDivider(color = Pots.Control)
            Column(verticalArrangement = Arrangement.spacedBy(10.dp), modifier = Modifier.fillMaxWidth().background(Pots.Surface).padding(16.dp)) {
                Text("續約網域", fontSize = 13.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
                Row(
                    modifier = Modifier.fillMaxWidth().border(1.dp, Pots.Border, RoundedCornerShape(6.dp)).padding(horizontal = 10.dp, vertical = 8.dp),
                ) {
                    Text(request.lineItem, fontSize = 12.sp, color = Pots.InkSoft, modifier = Modifier.weight(1f))
                    Text(request.amount, fontSize = 12.sp, color = Pots.InkSoft)
                }
                Box(Modifier.fillMaxWidth(0.7f).height(8.dp).background(Pots.Neutral, RoundedCornerShape(4.dp)))
                Box(Modifier.fillMaxWidth(0.5f).height(8.dp).background(Pots.Neutral, RoundedCornerShape(4.dp)))
                Box(Modifier.fillMaxWidth().padding(top = 4.dp), contentAlignment = Alignment.CenterEnd) {
                    Text(
                        request.target,
                        fontSize = 12.sp,
                        fontWeight = FontWeight.SemiBold,
                        color = Color.White,
                        modifier = Modifier
                            .targetOutline(Pots.Amber)
                            .background(Pots.Ink, RoundedCornerShape(6.dp))
                            .padding(horizontal = 14.dp, vertical = 8.dp),
                    )
                }
            }
        }
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Box(Modifier.size(12.dp).border(2.dp, Pots.Amber, RoundedCornerShape(3.dp)))
            Text("agent 要點擊的位置 · 截圖於 ${ageLabel(request.capturedMinutesAgo)}", fontSize = 12.sp, color = Pots.Muted)
        }
    }
}

/** 在元件外 3dp 畫一圈 3dp 的框，對應設計稿的 `outline-offset`。 */
private fun Modifier.targetOutline(color: Color): Modifier = drawWithContent {
    drawContent()
    val gap = 3.dp.toPx()
    val width = 3.dp.toPx()
    val inset = gap + width / 2
    drawRoundRect(
        color = color,
        topLeft = Offset(-inset, -inset),
        size = Size(size.width + inset * 2, size.height + inset * 2),
        cornerRadius = CornerRadius(9.dp.toPx()),
        style = Stroke(width),
    )
}
