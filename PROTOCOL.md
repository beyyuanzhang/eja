# EJA Host Protocol v1

`eja-host` 是 EJA 的不可变运行宿主，通过标准输入输出（newline-delimited JSON）与任意前端进程通信。

## 启动方式

```sh
eja-host --data-dir "<data_dir>"
```

默认数据目录为 `~/Library/Application Support/Eja/`。

## 数据目录边界

- 内核与用户数据使用同级目录。数据目录名为 `Eja` 时，内核目录为 `ai.eja.kernel`，其他目录名追加 `.kernel`。
- 内核目录保存 `subject.db`、`config.json`、HTTP MCP OAuth 凭证 `oauth.json`、单实例锁 `host.lock`（以及启用语义编码器时按需生成的衍生缓存 `memory.db`）。macOS Seatbelt 限制 Shell 与本地 stdio MCP 进程访问该目录。宿主迁移旧 `core/` 目录时持有实例锁。
- `subject.db` 保存主体事实日志与宿主元数据。用户消息、普通唤醒与守望命中均按事实 ID 处理。命中时，宿主原子地写入世界观察并排入主动回合。回复与回合进度原子结算。
- 首次从旧版本升级时，宿主自动将旧 `window.json`（含可选 `settled_through`）、`wakes.json` 与 `kernel_version` 文件迁入 `subject.db` 元数据槽并清理旧文件；旧 `window.json` 仅含 `anchor` 时，宿主根据日志末尾的触发记录或中断的行动恢复进度。
- 能力授权采用“OS 沙箱边界 + 宿主暴露即授权”契约。宿主使用固定目录句柄读写 `connectors.json` 和 `outputs/`，拒绝跟随符号链接。
- macOS 的 `promote_tool({name, file, equals?, contains?, not_equals?})` 注册有固定名称的本地检查（至少提供 `equals`、`contains`、`not_equals` 之一）。规则保存在宿主元数据，复用同名规则需要参数一致。宿主只读数据目录直属的普通文件，最多读取 4096 字节的 UTF-8 文本，忽略末尾换行后校验所有给定谓词。
- `wake({delay_seconds, reason, guard_tool, max_checks})` 可按间隔复查指定规则，默认最多 60 次。相同 `reason` 或相同 `guard_tool` 会覆盖旧预约；`delay_seconds: 0` 取消匹配预约。文件缺失或尚未匹配时，复查不写主体事实、不调用模型。读取出错时停止守望并报告错误。省略 `guard_tool` 时仍是普通提醒。
- `keep({text, checker})` 可绑定检查规则。绑定后，`release({concern, witness_fact_id})` 要求引用当前挂念版本之后的同一规则命中记录，并在该规则不再被其他活跃挂念引用时自动清理其剩余守望。旧脚本守望在启动时停用。旧版要求验收的挂念可用 `keep` 绑定新规则。
- 模型回合至少需要 256 字节的上下文窗口。设置低于下限时，宿主保留待处理请求，并在设置恢复后继续处理。
- `connectors.json` 保存 MCP 连接器数组，文件变更在运行期自动热加载。HTTP 连接器支持标准 MCP OAuth 2.1（RFC 9728 / RFC 8414 元数据发现、RFC 7591 动态客户端注册 + PKCE 本地回环回调，以及 RFC 8628 设备码授权流如 GitHub 官方 MCP）；OAuth Token 保存在内核目录 `oauth.json` 并由宿主自动注入与刷新，`connectors.json` 无需保存明文 Token。
- `ui/` 保存用户空间前端源码。运行 `ui/apply.sh` 编译生成单文件可执行程序 `ui/EjaUI`（不在数据目录生成第二个 `.app` 包），运行中的 `Eja.app` 在当前回合空闲（`status == "idle"`）时自动将 `ui/EjaUI` 原地热更新至自身界面二进制并无缝重启（保留 `EjaApp.factory` 出厂备份；启动时按住 `Option` 键自动恢复出厂默认界面）。

## 兼容性约定

协议版本 1.x 遵循增量演进原则。新版本 `eja-host` 仅追加可选字段或新事件类型，前端解码时应忽略未知字段和未知事件，未知指令由宿主返回 `error` 事件。

## 输入指令（Frontend -> Host，每行一条 JSON）

- 发送消息 `{"cmd":"send","text":"你好"}`
- 保存配置 `{"cmd":"save_config","config":{...}}`（支持按域部分更新：省略 `connectors` 不改动 `connectors.json`，省略 `providers`/`judge` 不改动模型配置；Provider 的 `api_key` 为空字符串时自动保留内核目录中已有密钥）
- 测试 Provider `{"cmd":"test_provider","id":"openai","provider":{...},"model":"gpt-4o"}`（`api_key` 为空时自动复用已保存密钥）
- 发起 HTTP 连接器 OAuth 授权 `{"cmd":"oauth_connector","id":"github"}`
- 立即重载连接器 `{"cmd":"reload_connectors"}`

## 输出事件（Host -> Frontend，每行一条 JSON）

- 状态快照 `{"event":"state","protocol_version":1,"kernel_version":"1.0.0","resources_dir":"...","status":"idle","status_detail":"在线","data_dir":"...","configured":true,"config":{...},"connector_statuses":[...],"stream":[...],"concerns":[...],"wakes":[...]}`
  `config.providers[].api_key` 始终为空字符串，`has_api_key` 表示是否已保存密钥。`stream[]` 可附带 `outcome_status` 与 `truncated`。`wakes[]` 可附带 `guard`，其中 `checker` 为规则名，`remaining` 为剩余检查次数。
- 回合内活动 `{"event":"activity","status_detail":"执行 Shell"}`。回合开始和结束仍发送状态快照。
- 流式文本增量 `{"event":"delta","text":"..."}`。用户回合中模型生成文本时实时推送增量，转入工具调用（`activity`）或回合结算（`state`）时收束。
- 单实例冲突 `{"event":"already_running","ui_pid":12345}`（同一数据目录已有运行中的 `eja-host` 时立即发出并退出，前端可激活已有实例后退出自身）
- Provider 测试结果 `{"event":"provider_test","id":"openai","ok":true,"message":"..."}`
- 系统通知 `{"event":"notify","title":"EJA","body":"..."}`
- 错误提示 `{"event":"error","message":"..."}`
