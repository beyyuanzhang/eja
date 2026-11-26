#!/usr/bin/env bash
set -euo pipefail

source_dir="$(cd "$(dirname "$0")" && pwd)"
data_dir="${1:-${eja_DATA_DIR:-"$HOME/Library/Application Support/eja"}}"
plugin_dir="$data_dir/capabilities/screen"
mkdir -p "$plugin_dir"
staged_bin="$(mktemp "$plugin_dir/.eja-screen.XXXXXX")"
trap 'rm -f "$staged_bin"' EXIT
xcrun swiftc -parse-as-library -O -target "$(uname -m)-apple-macosx14.0" \
  "$source_dir"/*.swift -o "$staged_bin"
chmod 755 "$staged_bin"

/usr/bin/python3 - "$data_dir" "$staged_bin" <<'PY'
import json
import os
import sys
import tempfile
from pathlib import Path

data_dir = Path(sys.argv[1]).resolve()
config = data_dir / "capabilities.json"
binary = data_dir / "capabilities/screen/eja-screen"
capabilities = json.loads(config.read_text(encoding="utf-8")) if config.exists() else []
point = {
    "type": "object",
    "properties": {axis: {"type": "number", "minimum": 0, "maximum": 1} for axis in ("x", "y")},
    "required": ["x", "y"],
    "additionalProperties": False,
}
entry = {
    "id": "screen",
    "name": "屏幕感知",
    "enabled": True,
    "transport": {
        "type": "exec",
        "command": str(binary),
        "description": "观察并操作 macOS 界面。{} 列出取景对象，source 获取截图，source+inspect 读取窗口控件及相对坐标。鼠标用 source 画面左上为原点的 0–1 坐标，键盘输入发给当前焦点。capture=true 可在动作后取图。",
        "schema": {
            "type": "object",
            "properties": {
                "source": {"type": "string", "description": "列表中的显示器或窗口 id"},
                "inspect": {"type": "boolean", "description": "读取指定窗口的控件文字和中心坐标"},
                "action": {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string", "enum": ["move", "click", "drag", "scroll", "key", "type"]},
                        "at": dict(point, description="鼠标位置"),
                        "to": dict(point, description="拖拽终点"),
                        "button": {"type": "string", "enum": ["left", "right"]},
                        "count": {"type": "integer", "minimum": 1, "maximum": 2, "description": "点击次数"},
                        "delta": {
                            "type": "object",
                            "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}},
                            "description": "滚动量，正 x 向右，正 y 向下",
                            "additionalProperties": False,
                        },
                        "key": {"type": "string", "description": "按键或快捷键，如 return、cmd+a、shift+left"},
                        "text": {"type": "string", "description": "输入到当前焦点的文字"},
                    },
                    "required": ["type"],
                    "additionalProperties": False,
                },
                "capture": {"type": "boolean", "description": "操作后返回指定 source 的新截图"},
            },
            "additionalProperties": False,
        },
    },
}

for index, capability in enumerate(capabilities):
    if capability["id"] == entry["id"]:
        transport = capability["transport"]
        if transport["type"] != "exec" or transport["command"] != str(binary):
            sys.exit("screen 已被其他能力使用")
        entry["enabled"] = capability.get("enabled", True)
        entry["name"] = capability["name"]
        capabilities[index] = entry
        break
else:
    capabilities.append(entry)

with tempfile.NamedTemporaryFile(
    mode="w", encoding="utf-8", dir=data_dir, prefix=".capabilities.", delete=False
) as staged_config:
    json.dump(capabilities, staged_config, ensure_ascii=False, indent=2)
    staged_config.write("\n")
try:
    os.replace(sys.argv[2], binary)
    os.replace(staged_config.name, config)
finally:
    Path(staged_config.name).unlink(missing_ok=True)
print(binary)
PY
