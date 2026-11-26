# EJA Host Protocol v2

`eja-host` 是 EJA 的不可变运行宿主，通过标准输入输出（newline-delimited JSON）与任意前端进程通信。

## 启动方式

```sh
eja-host --data-dir "<data_dir>"
```

默认数据目录为 `~/Library/Application Support/Eja/`。

## 数据目录边界

- 内核与用户数据使用同级目录。数据目录名为 `Eja` 时，内核目录为 `ai.eja.kernel`，其他目录名追加 `.kernel`。
- 内核目录保存 `subject.db`、`config.json`、HTTP MCP OAuth 凭证 `oauth.json`、单实例锁 `host.lock`（以及启用语义编码器时按需生成的衍生缓存 `memory.db`）。macOS Seatbelt 限制 Shell、exec 插件和 stdio MCP 进程访问该目录。
- `subject.db` 保存主体事实日志与宿主元数据。用户消息、普通唤醒与守望命中均按事实 ID 处理。命中时，宿主原子地写入世界观察并排入主动回合。回复与回合进度原子结算。
- 宿主在恢复时按照已保存的回合游标继续处理，将中断的执行记为未知。新主体使用 v4 日志格式。v5 实验反思事实仍可读取，实验线索作为历史保留。语义缓存会校验编码器与日志，失效时重建。
- 能力授权采用“OS 沙箱边界 + 宿主暴露即授权”契约。宿主使用固定目录句柄读写 `capabilities.json` 和 `outputs/`，拒绝跟随符号链接。关闭或改动能力后，宿主在下一次执行前作废旧工具定义。
- macOS 的 `promote_tool({name, file, equals?, contains?, not_equals?})` 注册有固定名称的本地检查（至少提供 `equals`、`contains`、`not_equals` 之一）。规则保存在宿主元数据，复用同名规则需要参数一致。宿主只读数据目录直属的普通文件，最多读取 4096 字节的 UTF-8 文本，忽略末尾换行后校验所有给定谓词。
- `wake({delay_seconds, reason, guard_tool, max_checks})` 可按间隔复查指定规则，默认最多 60 次。相同 `reason` 或相同 `guard_tool` 会覆盖旧预约；`delay_seconds: 0` 取消匹配预约。文件缺失或尚未匹配时，复查不写主体事实、不调用模型。读取出错时停止守望并报告错误。省略 `guard_tool` 时仍是普通提醒。
- `keep({text, checker})` 可绑定检查规则。绑定后，`release({concern, witness_fact_id})` 要求引用当前挂念版本之后的同一规则命中记录。通过校验的证据 ID 随 `Released` 事实一起写入。该规则无其他活跃挂念引用时会自动清理其剩余守望。
- 模型回合至少需要 256 字节的上下文窗口。设置低于下限时，宿主保留待处理请求，并在设置恢复后继续处理。
- `search` 与相关学习回放共用本地向量检索。macOS 包自带默认模型，`EJA_ONNX_MODEL_DIR` 可指定首选模型目录。模型、索引或向量查询不可用时使用关键词检索。
- 以 `EJA_RECALL_LEARNING=1` 启动宿主可启用实验性检索学习。模型可单独调用 `answer_with_receipt({text, receipt})` 结束回合，宿主验证该整数 ID 对应本回合检索已返回的已观测回执。候选线索必须来自首查事实，在两份独立回执的原快照和预算下提前召回目标，并保留首查的已观测回执。宿主用 `eja.recall` 的 `Acted/Returned` 提交更新。之后相同查询仅在学到的线索能保留原查询可见的已观测回执时采用它，并以 `used_cue` 标出线索。
- 检索结果超出工具预算时按排序保留装得下的完整事实，并标记 `truncated`。检索仍受返回条数与字节预算限制，日志增长会改变同一线索的可见结果。
- `capabilities.json` 保存统一能力数组（内嵌默认常驻的 `builtin` `shell` 能力、自由实现的 `exec` 本地插件能力，以及 `stdio` / `http` MCP 能力），文件变更在运行期自动热加载。`exec` 插件通过 `stdin` 接收参数 JSON、向 `stdout` 输出结果文本（退出码 `0` 为 `Observed`，非 `0` 为 `Failed`），零协议框架绑定。HTTP 能力支持标准 MCP OAuth 2.1（RFC 9728 / RFC 8414 元数据发现、RFC 7591 动态客户端注册 + PKCE 本地回环回调，以及 RFC 8628 设备码授权流；要求预注册 Client ID 的服务可在能力键值中提供 `X-OAuth-Client-Id`）；OAuth Token 保存在内核目录 `oauth.json` 并由宿主自动注入与刷新，`capabilities.json` 无需保存明文 Token。
- `ui/` 保存用户空间前端源码。运行 `ui/apply.sh` 会生成 `ui/libEjaUI.dylib`。EJA 空闲时沿用当前数据目录重启，启动器加载新的动态库。已安装的应用包保持原样。加载失败或启动时按住 Option 键会使用包内出厂界面。

`exec` 插件可直接向 `stdout` 输出图像，MCP 服务可通过 `image` 内容块返回图像。JPEG、PNG、GIF 和 WebP 共用模型图像接口。图像只发给收到该工具回执的下一步模型请求。主体日志只保存文字回执，模型请求会用 `images_omitted` 表明超出窗口而省略的图像数。

`stdio` 和 Streamable HTTP MCP 服务须支持 `2026-07-28`。OpenAI 和 Anthropic 官方端点的提示缓存参数由 Provider 适配器设置。

前端与 `eja-host` 使用同一构建的协议。

## 输入指令（Frontend -> Host，每行一条 JSON）

- 发送消息 `{"cmd":"send","text":"你好"}`
- 保存配置 `{"cmd":"save_config","id":"请求标识","config":{...}}`。各配置域可单独更新。带 `id` 时，保存后的状态快照会带回 `save_id`。空 `api_key` 只复用当前数据目录下同一 Provider ID、接口类型及 API 根地址的密钥。
- 测试 Provider `{"cmd":"test_provider","id":"openai","provider":{...},"model":"gpt-4o"}`。密钥复用规则与保存配置一致。
- 获取模型列表 `{"cmd":"list_models","id":"请求标识","provider":{...}}`。由宿主按当前数据目录和 Provider 配置请求。
- 发起 HTTP 能力 OAuth 授权 `{"cmd":"oauth_capability","id":"github"}`
- 立即重载能力 `{"cmd":"reload_capabilities"}`

## 输出事件（Host -> Frontend，每行一条 JSON）

- 状态快照 `{"event":"state","protocol_version":2,"kernel_version":"1.0.1","resources_dir":"...","status":"idle","status_detail":"在线","data_dir":"...","configured":true,"config":{...},"capability_statuses":[...],"stream":[...],"concerns":[...],"wakes":[...]}`
  `config.providers[].api_key` 始终为空字符串，`has_api_key` 表示是否已保存密钥。工具事件可附带 `outcome_status`、原始回执 ID `receipt_id` 与 `truncated`，这些字段由事实日志生成。`wakes[]` 可附带 `guard`，其中 `checker` 为规则名，`remaining` 为剩余检查次数。
- 回合内活动 `{"event":"activity","status_detail":"执行 Shell"}`。回合开始和结束仍发送状态快照。
- 流式文本增量 `{"event":"delta","text":"..."}`。用户回合中模型生成文本时实时推送增量，转入工具调用（`activity`）或回合结算（`state`）时收束。
- 单实例冲突 `{"event":"already_running","ui_pid":12345}`（同一数据目录已有运行中的 `eja-host` 时立即发出并退出，前端可激活已有实例后退出自身）
- Provider 测试结果 `{"event":"provider_test","id":"openai","ok":true,"message":"..."}`
- 模型列表 `{"event":"provider_models","id":"请求标识","models":["..."]}`。请求失败时返回 `error` 字段。
- 系统通知 `{"event":"notify","title":"EJA","body":"..."}`
- 错误提示 `{"event":"error","message":"..."}`
