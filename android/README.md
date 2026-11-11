# pots-android

Pots 手機客戶端（Kotlin + Jetpack Compose）。目前只有 UI，資料是假的，還沒接上任何服務。畫面依「Pots 手機端 UI」設計稿實作，配置依 [`proposal/ux-and-frontend.md`](../proposal/ux-and-frontend.md) 的手機欄位。

手機端已定案用原生 Android，推播走 UnifiedPush，由 ntfy 當 distributor（見 [`proposal/approvals-and-notifications.md`](../proposal/approvals-and-notifications.md#手機推播)）。推播與服務連線在 M8 接上。

## 執行

需要 JDK 17 以上與 Android SDK（platform 36）。`local.properties` 不進版控，第一次先指定 SDK 位置：

```sh
cd android
echo "sdk.dir=$ANDROID_HOME" > local.properties

./gradlew :app:installDebug          # 裝到已連線的裝置或模擬器
./gradlew :app:assembleDebug         # 只建 APK：app/build/outputs/apk/debug/
```

推播落地頁可以用 deep link 直接開：

```sh
adb shell am start -d pots://approval/1 dev.pots.mobile
adb shell am start -d pots://gui-approval/3 dev.pots.mobile
adb shell am start -d pots://usage dev.pots.mobile
```

## 已有的畫面

| 畫面 | 可以做的事 |
| --- | --- |
| 專案 | 實例切換器（選單列出實例、加入實例）、待處理橫幅（點了進 Inbox）、project 列表（狀態、最新摘要、待處理徽章）、已移交的 project |
| 對話 | agent 狀態與本週預算、停止；對話流（「你不在期間」、訊息、Goal 卡片、PR 事件、內嵌核准卡）；輸入框（送出、「排隊」或「改方向」，agent 忙碌時訊息標成排隊中或改方向） |
| 狀態抽屜 | 對話頁底部的把手點一下或往上拉。任務分頁：Goal 進度與預算、任務依狀態分組、PR 與 CI；記憶、自動化（開關）、電腦分頁 |
| Inbox | 全部／核准／需要你篩選；核准、拒絕、再試一次、看 thread；靜音時段提示 |
| 核准專頁 | 將被執行的動作、檢查結果、diff 摘要、附理由；`pots://approval/{id}` |
| GUI 核准 | 託管電腦截圖示意、要點擊的位置、接手畫面；`pots://gui-approval/{id}` |
| 總管 | 跨 project 的對話、專案概況（點了進該 project）、建議與輸入框 |
| 設定 | 實例、用量入口、通知、推播落地頁預覽（設計稿沒有，先放最少的內容） |
| 用量 | 今天／7 天／30 天切換；總 token（input／output）、run 數、估計金額與 CLI 回報金額；訂閱／API／CLI 分帳與主要工作／記憶整理（OM）比例；依 project、模型、Goal 彙總（Goal 預算以 token 計，顯示已用／預算，沒設顯示「不限」）；沒有單價的 token 數提示。從設定進入或 `pots://usage`。資料形狀照 `pots-proto` 的 `CostReport` |

同一筆核准在 Inbox、對話卡片、狀態抽屜、核准專頁任一處決定後，其他地方、底部 Inbox 徽章與專案列表的橫幅一起更新。「搜尋」「新增專案」「看完整 diff」「任務 thread」「接手畫面」等還沒實作，點了會在底部提示。

## 結構

- `model/Model.kt`：view model 與 `ClientCommand`，命名對齊 `crates/pots-desktop/src/model.rs`。
- `model/Mock.kt`：假資料，內容取自設計稿。
- `model/PotsStore.kt`：客戶端的整體狀態（Compose snapshot state），使用者動作會把要送出的指令記在 `PotsStore.outbox`。
- `PotsApp.kt`：導航（Navigation Compose）、底部分頁、deep link 與底部提示。
- `ui/screens/`：各畫面；`StatusSheet.kt` 是對話頁的上拉抽屜。
- `ui/components/`：依設計稿手刻的小元件；`Decisions.kt` 是共用的核准按鈕列。
- `ui/icons/PotsIcons.kt`：設計稿的 SVG path 原樣轉成 `ImageVector`。
- `ui/theme/Theme.kt`：色票與字級。設計稿只有淺色；字型先用系統字型（設計稿是 Inter + Noto Sans TC）。

## 驗證

```sh
./gradlew :app:testDebugUnitTest                                   # store 單元測試與 Compose 互動測試（Robolectric）
./gradlew :app:testDebugUnitTest --tests '*ScreenshotTest' -Proborazzi.test.record=true
```

第二行用 Roborazzi 輸出 390×844dp 的畫面 PNG 到 `app/build/outputs/roborazzi/`，方便和設計稿對照，不是 golden-image 比對。Robolectric 第一次執行會從 Maven Central 下載 `android-all-instrumented`。
