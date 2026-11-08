# Pots worker 的容器 image（proposal/agent-runtime.md#worker-adapter內建-worker-與外部-coding-cli）。
#
#   docker build -t pots-worker:latest docker/worker
#
# 內建 worker 與 Claude Code worker 共用：git、常用建置工具（含 coreutils 的 timeout），加上未修改的
# 官方 Claude Code CLI（釘選版本；升級時改 CLAUDE_CODE_VERSION 並重跑 worker 的測試）。Claude Code
# 的登入狀態不在 image 裡：Pots 把資料目錄的 `claude-config` 掛到 /claude-config（CLAUDE_CONFIG_DIR），
# 由你自己官方登入（設定的「Worker」類別有指令）；每個任務另有自己的 home（/home/pots）。image 裡
# 沒有任何憑證，Pots 也不會把 GitHub token 交給容器。
FROM node:22-bookworm-slim

ARG CLAUDE_CODE_VERSION=2.1.319

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git ca-certificates curl ripgrep build-essential python3 \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
    && npm cache clean --force \
    # 容器以你的 uid 執行（--user），entrypoint 要能補上 passwd 項目。
    && chmod 0666 /etc/passwd \
    && mkdir -p /home/pots /workspace /run/pots /claude-config \
    && chmod 0777 /home/pots /workspace /claude-config

COPY entrypoint.sh /usr/local/bin/pots-entrypoint
RUN chmod 0755 /usr/local/bin/pots-entrypoint

ENV HOME=/home/pots \
    DISABLE_AUTOUPDATER=1
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/pots-entrypoint"]
CMD ["sleep", "infinity"]
