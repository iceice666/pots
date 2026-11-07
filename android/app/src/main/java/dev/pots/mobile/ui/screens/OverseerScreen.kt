package dev.pots.mobile.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pots.mobile.LocalNotice
import dev.pots.mobile.PotsNav
import dev.pots.mobile.model.OverseerItem
import dev.pots.mobile.model.PotsStore
import dev.pots.mobile.ui.components.Composer
import dev.pots.mobile.ui.components.GroupedList
import dev.pots.mobile.ui.components.HeaderBar
import dev.pots.mobile.ui.components.IconAction
import dev.pots.mobile.ui.components.IconTile
import dev.pots.mobile.ui.components.InboxKindTag
import dev.pots.mobile.ui.components.Pill
import dev.pots.mobile.ui.icons.PotsIcons
import dev.pots.mobile.ui.theme.Pots
import java.time.LocalTime
import java.time.format.DateTimeFormatter

/** 總管：同一個實例內跨 project 的永久對話。 */
@Composable
fun OverseerScreen(store: PotsStore, nav: PotsNav) {
    val notice = LocalNotice.current
    val items = store.overseer
    var draft by rememberSaveable { mutableStateOf("") }
    val listState = rememberLazyListState()
    val send: (String) -> Unit = { text ->
        store.askOverseer(text, LocalTime.now().format(DateTimeFormatter.ofPattern("HH:mm")))
    }

    LaunchedEffect(items.size) { if (items.isNotEmpty()) listState.animateScrollToItem(items.lastIndex) }

    Column(Modifier.fillMaxSize().background(Pots.Surface)) {
        HeaderBar {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                modifier = Modifier.statusBarsPadding().padding(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 12.dp),
            ) {
                IconTile(PotsIcons.Compass, size = 36.dp, iconSize = 20.dp, radius = 10.dp)
                Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    Text("總管", fontSize = 20.sp, fontWeight = FontWeight.Bold, color = Pots.Ink)
                    Text("${store.instance.name} · ${store.projects.size} 個專案 · 閒置", fontSize = 12.sp, color = Pots.Muted)
                }
                IconAction(PotsIcons.Panel, "專案概況與記憶", { notice("專案概況與記憶還沒實作") })
            }
        }

        LazyColumn(
            state = listState,
            contentPadding = PaddingValues(16.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
            modifier = Modifier.weight(1f).fillMaxWidth().background(Pots.Bg),
        ) {
            itemsIndexed(items) { index, item ->
                val continues = item !is OverseerItem.UserMsg && items.getOrNull(index - 1).let { it != null && it !is OverseerItem.UserMsg }
                when (item) {
                    is OverseerItem.UserMsg -> OverseerUserBubble(item)
                    is OverseerItem.Reply -> OverseerRow(showAvatar = !continues) {
                        Text(item.text, fontSize = 15.sp, lineHeight = 23.sp, color = Pots.Ink)
                    }
                    is OverseerItem.Overview -> OverseerRow(showAvatar = !continues) {
                        GroupedList(item.rows) { row ->
                            Column(
                                verticalArrangement = Arrangement.spacedBy(3.dp),
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .clickable(role = Role.Button) { nav.channel(row.project) }
                                    .padding(horizontal = 14.dp, vertical = 12.dp),
                            ) {
                                Row(verticalAlignment = Alignment.CenterVertically) {
                                    Text("# ${store.project(row.project).name}", fontSize = 14.sp, fontWeight = FontWeight.Bold, color = Pots.Ink, modifier = Modifier.weight(1f))
                                    InboxKindTag(row.kind, short = true, fontSize = 11.sp)
                                }
                                Text(row.text, fontSize = 13.sp, color = Pots.Muted)
                            }
                        }
                    }
                }
            }
        }

        HorizontalDivider(color = Pots.Border)
        Column(
            verticalArrangement = Arrangement.spacedBy(10.dp),
            modifier = Modifier
                .windowInsetsPadding(WindowInsets.ime)
                .padding(start = 12.dp, end = 12.dp, top = 10.dp, bottom = 8.dp),
        ) {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.horizontalScroll(rememberScrollState())) {
                store.overseerSuggestions.forEach { suggestion ->
                    Pill(suggestion, selected = false, onClick = { send(suggestion) }, role = Role.Button)
                }
            }
            Composer(
                value = draft,
                onValueChange = { draft = it },
                placeholder = "問總管任何專案的事",
                onSend = {
                    send(draft)
                    draft = ""
                },
            )
        }
    }
}

@Composable
private fun OverseerUserBubble(msg: OverseerItem.UserMsg) {
    BoxWithConstraints(Modifier.fillMaxWidth()) {
        val max = maxWidth * 0.82f
        Column(horizontalAlignment = Alignment.End, verticalArrangement = Arrangement.spacedBy(4.dp), modifier = Modifier.fillMaxWidth()) {
            Text(
                msg.text,
                color = Color.White,
                fontSize = 15.sp,
                lineHeight = 22.sp,
                modifier = Modifier
                    .widthIn(max = max)
                    .background(Pots.Accent, RoundedCornerShape(16.dp, 16.dp, 4.dp, 16.dp))
                    .padding(horizontal = 14.dp, vertical = 10.dp),
            )
            Text(msg.time, fontSize = 11.sp, color = Pots.Muted)
        }
    }
}

@Composable
private fun OverseerRow(showAvatar: Boolean, content: @Composable () -> Unit) {
    Row(horizontalArrangement = Arrangement.spacedBy(10.dp), modifier = Modifier.fillMaxWidth()) {
        if (showAvatar) IconTile(PotsIcons.Compass) else Spacer(Modifier.padding(start = 30.dp))
        Box(Modifier.weight(1f)) { content() }
    }
}
