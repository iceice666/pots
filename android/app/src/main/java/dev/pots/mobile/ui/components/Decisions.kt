package dev.pots.mobile.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.LocalNotice
import dev.pots.mobile.model.ApprovalId
import dev.pots.mobile.model.Decision
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Pots

/** 已決定的核准：取代按鈕列，顯示結果。 */
@Composable
fun DecisionResult(decision: Decision, modifier: Modifier = Modifier, height: Dp = 48.dp) {
    val approved = decision == Decision.Approved
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterHorizontally),
        modifier = modifier
            .fillMaxWidth()
            .height(height)
            .background(if (approved) Pots.SuccessSoft else Pots.Neutral, RoundedCornerShape(10.dp))
            .padding(horizontal = 12.dp),
    ) {
        Icon(
            if (approved) PotsIcons.Check else PotsIcons.Close,
            contentDescription = null,
            tint = if (approved) Pots.Success else Pots.InkSoft,
            modifier = Modifier.size(18.dp),
        )
        Text(
            decision.label,
            color = if (approved) Pots.Success else Pots.InkSoft,
            fontSize = 15.sp,
            fontWeight = FontWeight.SemiBold,
        )
    }
}

/**
 * 核准／拒絕按鈕列。決定後改顯示 [DecisionResult]，所有用到同一筆核准的地方一起更新。
 * `trailing` 放在按鈕後面，例如對話卡片上的「看詳情」。
 */
@Composable
fun ApprovalButtons(
    store: PotsStore,
    approval: ApprovalId,
    modifier: Modifier = Modifier,
    height: Dp = 48.dp,
    approveLabel: String = "核准",
    note: () -> String? = { null },
    /** 專頁的底部決定列：拒絕在左，較寬的核准在右。 */
    reversed: Boolean = false,
    trailing: (@Composable () -> Unit)? = null,
) {
    val notice = LocalNotice.current
    val decided = store.decision(approval)
    if (decided != null) {
        DecisionResult(decided, modifier, height)
        return
    }
    val title = store.approval(approval)?.title ?: store.guiApproval(approval)?.title.orEmpty()
    val fontSize = when {
        height < 48.dp -> 15.sp
        height > 48.dp -> 17.sp
        else -> 16.sp
    }
    val radius = if (height > 48.dp) 12.dp else 10.dp
    Row(horizontalArrangement = Arrangement.spacedBy(if (height < 48.dp) 8.dp else 10.dp), modifier = modifier.fillMaxWidth()) {
        val approve = @Composable {
            PrimaryButton(
                approveLabel,
                onClick = { if (store.decide(approval, Decision.Approved, note())) notice("已核准：$title") },
                height = height,
                radius = radius,
                fontSize = fontSize,
                modifier = Modifier.weight(if (reversed) 1.4f else 1f),
            )
        }
        val reject = @Composable {
            SecondaryButton(
                "拒絕",
                onClick = { if (store.decide(approval, Decision.Rejected, note())) notice("已拒絕：$title") },
                height = height,
                radius = radius,
                fontSize = fontSize,
                modifier = Modifier.weight(1f),
            )
        }
        if (reversed) {
            reject()
            approve()
        } else {
            approve()
            reject()
        }
        trailing?.invoke()
    }
}
