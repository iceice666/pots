#!/bin/sh
# 以任意 uid 執行時補上 passwd 項目（有些工具，例如 Node 的 os.userInfo，沒有它會失敗）。
if ! whoami >/dev/null 2>&1; then
    echo "pots:x:$(id -u):$(id -g):Pots worker:${HOME:-/home/pots}:/bin/sh" >> /etc/passwd
fi
exec "$@"
