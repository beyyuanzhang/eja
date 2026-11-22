# EJA Host Protocol v1

`eja-host` 是 EJA 的不可变运行宿主，通过标准输入输出（newline-delimited JSON）与任意前端进程通信。

## 启动方式

```sh
eja-host --data-dir "<data_dir>"
```

默认数据目录为 `~/Library/Application Support/Eja/`。

## 数据目录边界

- `core/` 由 macOS Seatbelt 保护，沙箱内 `shell` 与 MCP 进程无权读写，包含 `subject.db`、`config.json`、`wakes.json`、`window.json`、`kernel_version` 与单实例文件锁 `host.lock`。
- `connectors.json` 保存 MCP 连接器数组，文件变更在运行期自动热加载。
- `ui/` 保存用户空间前端。官方启动器按统一约定执行 `ui/Eja.app/Contents/MacOS/EjaApp`（默认由 `ui/apply.sh` 编译原生 SwiftUI 并封装为完整 `ui/Eja.app` 应用包，也可替换为任意符合该入口路径的可执行程序或脚本），并传入 `EJA_DATA_DIR`、`EJA_HOST_BIN`、`EJA_UI_TEMPLATE_DIR` 与 `EJA_FALLBACK_APP_BIN`。启动 `Eja.app` 时按住 `Option` 键可直接进入官方默认界面。

## 兼容性约定

协议版本 1.x 遵循增量演进原则。新版本 `eja-host` 仅追加可选字段或新事件类型，前端解码时应忽略未知字段和未知事件，未知指令由宿主返回 `error` 事件。

## 输入指令（Frontend -> Host，每行一条 JSON）

- 发送消息 `{"cmd":"send","text":"你好"}`
- 保存配置 `{"cmd":"save_config","config":{...}}`（Provider 的 `api_key` 为空字符串时自动保留 `core/config.json` 中已有密钥）
- 测试 Provider `{"cmd":"test_provider","id":"openai","provider":{...},"model":"gpt-4o"}`（`api_key` 为空时自动复用已保存密钥）
- 立即重载连接器 `{"cmd":"reload_connectors"}`

## 输出事件（Host -> Frontend，每行一条 JSON）

- 状态快照 `{"event":"state","protocol_version":1,"kernel_version":"1.0.0","resources_dir":"...","status":"idle","status_detail":"在线","data_dir":"...","configured":true,"config":{...},"connector_statuses":[...],"stream":[...],"concerns":[...],"wakes":[...]}`
  其中 `config.providers[].api_key` 始终返回空字符串，通过 `has_api_key` 布尔字段表示是否已保存密钥。
- 单实例冲突 `{"event":"already_running","ui_pid":12345}`（同一数据目录已有运行中的 `eja-host` 时立即发出并退出，前端可激活已有实例后退出自身）
- Provider 测试结果 `{"event":"provider_test","id":"openai","ok":true,"message":"..."}`
- 系统通知 `{"event":"notify","title":"EJA","body":"..."}`
- 错误提示 `{"event":"error","message":"..."}`
