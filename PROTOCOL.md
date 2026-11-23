# EJA Host Protocol v1

`eja-host` 是 EJA 的不可变运行宿主，通过标准输入输出（newline-delimited JSON）与任意前端进程通信。

## 启动方式

```sh
eja-host --data-dir "<data_dir>"
```

默认数据目录为 `~/Library/Application Support/Eja/`。

## 数据目录边界

- 内核与用户数据使用同级目录。数据目录名为 `Eja` 时，内核目录为 `ai.eja.kernel`，其他目录名追加 `.kernel`。
- 内核目录保存 `subject.db`、`config.json`、`window.json`、`kernel_version` 与单实例锁 `host.lock`。macOS Seatbelt 限制 Shell 与本地 stdio MCP 进程访问该目录。宿主迁移旧 `core/` 目录时持有实例锁。
- `subject.db` 保存主体日志、唤醒计划和已处理的触发 ID。宿主按 ID 顺序处理用户消息与到期提醒。每个回合在同一事务中写入回复并推进处理进度。主动沉默只推进进度。新回复记录本次触发的来源。
- `window.json` 保存上下文窗口起点 `anchor`。首次升级时，宿主导入旧文件中的 `settled_through`。旧文件仅含 `anchor` 时，宿主根据日志末尾的触发记录或中断的行动恢复进度。
- 能力授权采用“OS 沙箱边界 + 宿主暴露即授权”契约：宿主暴露给模型的工具列表（`wake`、`shell` 与已启用的 MCP 连接器）即为当次行动授权集合；宿主启动时固定数据目录与内核目录句柄，读写 `connectors.json` 与 `outputs/` 时基于固定目录句柄且不跟随符号链接，防止沙箱内进程替换目录或创建符号链接越界写入内核目录或应用包。
- 模型回合至少需要 256 字节的上下文窗口。设置低于下限时，宿主保留待处理请求，并在设置恢复后继续处理。
- `connectors.json` 保存 MCP 连接器数组，文件变更在运行期自动热加载。
- `ui/` 保存用户空间前端。运行 `ui/apply.sh` 发布 `ui/Eja.app` 后，宿主在与 Shell 相同的 macOS 沙箱中启动 `Contents/MacOS/EjaApp`，并通过其标准输入输出连接同一个主体。前端读入状态和事件，向标准输出写指令，环境变量 `EJA_ATTACHED_UI=1` 表示已有宿主。自定义前端可以发消息与重载用户连接器，编辑或测试模型配置由官方设置窗口处理。启动官方 App 时按住 `Option` 键进入默认界面。

## 兼容性约定

协议版本 1.x 遵循增量演进原则。新版本 `eja-host` 仅追加可选字段或新事件类型，前端解码时应忽略未知字段和未知事件，未知指令由宿主返回 `error` 事件。

## 输入指令（Frontend -> Host，每行一条 JSON）

- 发送消息 `{"cmd":"send","text":"你好"}`
- 保存配置 `{"cmd":"save_config","config":{...}}`（支持按域部分更新：省略 `connectors` 不改动 `connectors.json`，省略 `providers`/`judge` 不改动模型配置；Provider 的 `api_key` 为空字符串时自动保留内核目录中已有密钥）
- 测试 Provider `{"cmd":"test_provider","id":"openai","provider":{...},"model":"gpt-4o"}`（`api_key` 为空时自动复用已保存密钥）
- 立即重载连接器 `{"cmd":"reload_connectors"}`
- 官方界面请求接入自定义前端 `{"cmd":"launch_ui"}`。自定义前端读到首个状态快照后发送 `{"cmd":"ui_ready"}`，宿主向官方界面发送 `ui_ready`（含 `pid`）或在断开时发送 `ui_closed`。自定义前端可发送 `{"cmd":"open_settings"}` 请求官方设置窗口。

## 输出事件（Host -> Frontend，每行一条 JSON）

- 状态快照 `{"event":"state","protocol_version":1,"kernel_version":"1.0.0","resources_dir":"...","status":"idle","status_detail":"在线","data_dir":"...","configured":true,"config":{...},"connector_statuses":[...],"stream":[...],"concerns":[...],"wakes":[...]}`
  其中 `config.providers[].api_key` 始终返回空字符串，通过 `has_api_key` 布尔字段表示是否已保存密钥。
- 回合内活动 `{"event":"activity","status_detail":"执行 Shell"}`。回合开始和结束仍发送状态快照。
- 单实例冲突 `{"event":"already_running","ui_pid":12345}`（同一数据目录已有运行中的 `eja-host` 时立即发出并退出，前端可激活已有实例后退出自身）
- Provider 测试结果 `{"event":"provider_test","id":"openai","ok":true,"message":"..."}`
- 系统通知 `{"event":"notify","title":"EJA","body":"..."}`
- 错误提示 `{"event":"error","message":"..."}`
