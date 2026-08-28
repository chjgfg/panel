#!/bin/bash
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

"$SCRIPT_DIR/stop.sh"

# 编译哪个档跟着 PROFILE 走，默认 release
if [ "${PROFILE:-release}" = "debug" ]; then
    echo "编译 $APP（debug）..."
    build=(cargo build)
else
    echo "编译 $APP（release）..."
    build=(cargo build --release)
fi

# 编译失败必须停下。否则 target/ 里还是上一次编译的旧二进制，
# start.sh 检查文件存在就通过，会把旧版本默默跑起来 ——
# 「改了代码、重启了、行为没变」这种问题最难查
if ! "${build[@]}"; then
    echo "编译失败，不启动。旧进程已经停了，修好后跑 ./sh/start.sh"
    exit 1
fi

"$SCRIPT_DIR/start.sh"
