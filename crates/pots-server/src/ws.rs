//! WebSocket 連線：`Resume` 之後送快照或補送事件，接著即時推送。
//!
//! 順序保證：先訂閱廣播，再在日誌鎖下讀快照或補送的事件；之後廣播收到的事件依 `seq`
//! 去重，`Delta` 依串流位置去重（見 [`crate::hub`]）。跟不上廣播的連線直接關閉，
//! 客戶端帶 `last_seq` 重連即可補齊。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt, StreamExt};
use pots_proto::{ClientCommand, ServerEvent};
use tokio::sync::{broadcast, mpsc};

use crate::App;
use crate::hub::Envelope;

/// 連線後等第一個 `Resume` 的時間。
const RESUME_TIMEOUT: Duration = Duration::from_secs(30);

/// 自訂的 close code：事件跟不上，請重連。
const CLOSE_LAGGED: u16 = 4000;

pub(crate) async fn serve(app: Arc<App>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    let last_seq = match tokio::time::timeout(RESUME_TIMEOUT, next_text(&mut stream)).await {
        Ok(Some(text)) => match serde_json::from_str::<ClientCommand>(&text) {
            Ok(ClientCommand::Resume { last_seq }) => last_seq,
            _ => {
                let err = error_json("第一個指令必須是 Resume");
                let _ = sink.send(Message::Text(err.into())).await;
                let _ = sink.send(Message::Close(None)).await;
                return;
            }
        },
        _ => return,
    };

    // 先訂閱再讀日誌，兩者之間的事件不會漏掉。
    let mut rx = app.hub.subscribe();
    let (initial, mut cursor) = match app.hub.initial(last_seq).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "無法準備連線的初始狀態");
            let _ = sink.send(Message::Close(None)).await;
            return;
        }
    };
    for frame in initial.frames {
        if sink.send(Message::Text(frame.into())).await.is_err() {
            return;
        }
    }
    let mut stream_pos = initial.stream_pos;

    let (reply_tx, mut reply_rx) = mpsc::channel::<ServerEvent>(32);

    let reader = {
        let app = app.clone();
        async move {
            while let Some(text) = next_text(&mut stream).await {
                let app = app.clone();
                let reply = reply_tx.clone();
                // 指令在獨立 task 執行：連線中途斷掉也不會把寫到一半的指令丟掉。
                let _ =
                    tokio::spawn(async move { crate::commands::handle(&app, &text, &reply).await })
                        .await;
            }
        }
    };

    let writer = async move {
        loop {
            let frame: String = tokio::select! {
                _ = app.shutdown.cancelled() => {
                    let _ = sink.send(Message::Close(None)).await;
                    return;
                }
                ev = rx.recv() => match ev {
                    Ok(Envelope::Logged { seq, json, done }) => {
                        if seq <= cursor {
                            continue;
                        }
                        cursor = seq;
                        if let Some(item) = done {
                            stream_pos.remove(&item);
                        }
                        json.to_string()
                    }
                    Ok(Envelope::Delta { item, end, json }) => {
                        if stream_pos.get(&item).is_some_and(|pos| end <= *pos) {
                            continue;
                        }
                        json.to_string()
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::info!(skipped = n, "連線跟不上事件，關閉讓客戶端重連");
                        let _ = sink
                            .send(Message::Close(Some(CloseFrame {
                                code: CLOSE_LAGGED,
                                reason: "跟不上事件，請重連".into(),
                            })))
                            .await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                ev = reply_rx.recv() => match ev {
                    Some(ev) => match serde_json::to_string(&ev) {
                        Ok(json) => json,
                        Err(_) => continue,
                    },
                    None => return,
                },
            };
            if sink.send(Message::Text(frame.into())).await.is_err() {
                return;
            }
        }
    };

    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
}

/// 下一則文字訊息；連線關閉時回 `None`。
async fn next_text(stream: &mut futures::stream::SplitStream<WebSocket>) -> Option<String> {
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(text)) => return Some(text.to_string()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
    None
}

fn error_json(message: &str) -> String {
    serde_json::to_string(&ServerEvent::Error {
        message: message.into(),
    })
    .unwrap_or_default()
}
