# Phase 3：多裝置與復原

## 1. 目標

使用者能新增與撤銷裝置；Agent Host 主機毀損時，僅憑 recovery key 與遠端物件儲存（S3 相容，例如 Cloudflare R2）上的備份即可重建 Agent Host。

## 2. 範圍

**納入範圍**

- 裝置註冊表：由已信任的裝置掃描 QR code 並簽署新裝置的公鑰；hub 寫入註冊表，新裝置之後以自身金鑰連線
- 撤銷裝置：由任一台已信任裝置簽署撤銷；hub 立即中斷連線並拒絕其後續連線。Node 上的 `owner_device_keys` 由使用者於 node 上手動移除，client 提示須修改的 node
- 離線 recovery key：產生並離線保存；用於重建裝置信任與解密備份
- 加密備份：Agent Host 的 host 金鑰、agent 金鑰、記憶庫與稽核日誌（含 node 同步回來的部分），以 recovery key 加密後定期上傳至遠端物件儲存；儲存後端可設定
- 還原流程：於另一台機器僅憑 recovery key + 遠端備份重建 Agent Host

**不納入範圍**：行動版（P4；本階段的「新裝置」以第二台桌面 client 驗證）、connector、多人功能。

Node 僅能收到發送給自身的指令，此性質由 hub 依連線身分路由保證（已於 P2 驗證）。

## 3. 里程碑

- [ ] M3.1 裝置註冊表 + QR code 加入
- [ ] M3.2 裝置撤銷（hub 斷線 + 拒絕連線）+ 於 node 手動更新 `owner_device_keys` 的流程
- [ ] M3.3 Recovery key 的產生與離線保存流程
- [ ] M3.4 加密備份至遠端物件儲存（排程、保留份數）
- [ ] M3.5 以 recovery key + 遠端備份還原

## 4. 關卡

**路線圖關卡**

- 遠端儲存上僅有密文，未持有 recovery key 者無法解密
- 新裝置能以 recovery key 復原

**補充驗收測試**

- 遭撤銷的裝置立即斷線且無法再連線；node 更新政策後，其簽署的核准遭拒絕
- 演練：不變動 Agent Host 主機，於另一台機器僅憑 recovery key + 遠端備份重建 Agent Host，agent 身分與記憶完整；client 因 host 指紋不變而直接接受
- 遠端儲存上的備份檔無法以錯誤金鑰解密，遭竄改後解密失敗
