# ClaudeCode 植入 Codex 對話能力技術文件

## 目的

這份文件整理 Codex repo 內和以下需求直接相關的程式碼位置與整合方式：

- 在 ClaudeCode 的特定模式下，改走 Codex 的對話執行鏈
- 使用 Codex 既有 OAuth / ChatGPT 登入流程
- 使用 Codex 既有 backend / thread / turn / event stream
- 盡量不要在 ClaudeCode 端重做一套 token refresh、thread state、event protocol

這份文件同時回答兩個問題：

1. 這部分的程式碼檔案在哪裡
2. 最推薦的植入方式是什麼

## 先講結論

如果你的目標是「讓 ClaudeCode 某個 mode 直接拿 Codex 來跑對話」，最穩的切入點不是直接呼叫低階 ChatGPT backend REST，也不是硬讀 `auth.json`。

最推薦的路徑是：

1. 在 ClaudeCode 內新增一個 `codex-backed mode`
2. 該 mode 啟動 `codex app-server`
3. 用 app-server 的 JSON-RPC 介面做：
   - `initialize`
   - `account/login/start`
   - `thread/start`
   - `turn/start`
   - 接收通知流
4. 把 Codex 的通知事件轉成 ClaudeCode UI 需要的訊息格式

原因很直接：

- OAuth、workspace/account、token refresh、thread lifecycle、turn lifecycle、streaming event，Codex 已經在 app-server 這層包好了
- 直接打低階 backend 會少掉大量 Codex 既有邏輯
- 直接讀 `auth.json` 也不可靠，因為 Codex 可能把 auth 存在 keyring，而不是檔案

如果你必須讓 ClaudeCode 自己掌握 OAuth UI 或自己管理 access token，則可以走 app-server 的外部 token 注入路徑，但那條 API 在 repo 裡明確標了 internal / unstable，維護成本會高一些。

## 現有文件狀態

repo 內和 auth/config 直接相關的文件很少：

- `docs/authentication.md` 只有一行，指向外部文件
- `docs/config.md` 也是外部文件入口，沒有把 ClaudeCode 植入場景講清楚

所以目前 repo 內沒有一份「ClaudeCode 接 Codex 對話能力」的現成技術文件。這份文件就是補這個缺口。

## 程式碼檔案地圖

### A. OAuth / 登入 / token 儲存

這一層決定「怎麼登入」、「token 存在哪」、「怎麼 refresh」。

- `codex-rs/cli/src/login.rs`
  - CLI `codex login` 入口
  - 負責 browser login、device code login、api key login 的命令路由

- `codex-rs/login/src/server.rs`
  - 本地 OAuth callback server
  - 建立 authorize URL
  - 收 `/auth/callback`
  - 用 authorization code 交換 tokens
  - 把 tokens 寫入 auth storage

- `codex-rs/login/src/device_code_auth.rs`
  - device code flow
  - 先拿 user code，再輪詢 token，最後走同一組 token persistence

- `codex-rs/login/src/auth/manager.rs`
  - `CodexAuth` 核心抽象
  - 從 storage 載入 auth
  - 取得 bearer token / account id
  - refresh token
  - 支援 `ChatgptAuthTokens` 這種外部注入 token 的模式

- `codex-rs/login/src/auth/storage.rs`
  - auth storage backend
  - 支援 `file` / `keyring` / `auto` / `ephemeral`
  - 關鍵結論：不能假設 token 一定在 `auth.json`

- `codex-rs/login/src/token_data.rs`
  - `TokenData` 與 JWT claims 解析
  - 會從 id_token / access_token 提取 `chatgpt_account_id`、`chatgpt_plan_type`、`chatgpt_user_id`

- `codex-rs/login/src/auth/default_client.rs`
  - 共用 HTTP client / default headers / user-agent / originator
  - 很多走 ChatGPT / Codex backend 的 client 都會共用這層

### B. 給外部客戶端用的登入橋接

如果 ClaudeCode 不直接呼叫 `codex login`，而是想把登入流程嵌進自己的 UI，入口在 app-server。

- `codex-rs/app-server-protocol/src/protocol/v2.rs`
  - `LoginAccountParams`
  - `LoginAccountResponse`
  - `ChatgptAuthTokensRefreshParams`
  - `ChatgptAuthTokensRefreshResponse`

- `codex-rs/app-server-protocol/src/protocol/common.rs`
  - method name 定義
  - `account/login/start`
  - `account/login/cancel`
  - `account/logout`
  - `account/chatgptAuthTokens/refresh`

- `codex-rs/app-server/src/codex_message_processor.rs`
  - `login_chatgpt_v2`
  - `login_chatgpt_device_code_v2`
  - `login_chatgpt_auth_tokens`
  - 這裡是真正把 app-server login request 接到 `codex_login` 的地方

- `codex-rs/app-server/src/message_processor.rs`
  - `ExternalAuthRefreshBridge`
  - 當 app-server 使用外部注入的 ChatGPT tokens 時，這裡會反向向 client 要 refresh

### C. 對話 thread / turn / stream protocol

如果你的目標是「對話」，真正需要看的不是 backend-client，而是這組。

- `codex-rs/app-server-protocol/src/protocol/v2.rs`
  - `ThreadStartParams`
  - `TurnStartParams`
  - `ThreadStartedNotification`
  - `TurnStartedNotification`
  - `RawResponseItemCompletedNotification`
  - `AgentMessageDeltaNotification`

- `codex-rs/app-server-protocol/src/protocol/common.rs`
  - method name 與 notification name
  - `thread/start`
  - `turn/start`
  - `thread/started`
  - `turn/started`
  - `item/agentMessage/delta`

- `codex-rs/app-server/src/codex_message_processor.rs`
  - `thread_start`
  - `turn_start`
  - `submit_core_op`
  - 這裡是 app-server 把外部 JSON-RPC 請求轉成 core `Op` 的關鍵層

- `codex-rs/core/src/thread_manager.rs`
  - `ThreadManager`
  - `start_thread`
  - `start_thread_with_tools_and_service_name`
  - 建立實際的 Codex thread

- `codex-rs/core/src/codex_thread.rs`
  - `CodexThread`
  - `steer_input`
  - `validate_turn_context_overrides`
  - `inject_response_items`

- `codex-rs/core/src/session/mod.rs`
  - thread session / turn 提交流程
  - `steer_input`
  - `set_app_server_client_info`
  - `thread_config_snapshot`

### D. backend / ChatGPT 專用 client

這層是和第一方 backend 溝通的工具，但它不是你做多輪對話整合時的最佳直接入口。

- `codex-rs/backend-client/src/client.rs`
  - 會自動把 `chatgpt.com` 正規化到 `/backend-api`
  - 會組 `Authorization: Bearer ...`
  - 會附 `ChatGPT-Account-Id`
  - 目前更偏向 account / task / config / requirements / credits 等 backend API

- `codex-rs/chatgpt/src/chatgpt_client.rs`
  - 較薄的 ChatGPT backend GET wrapper

- `codex-rs/chatgpt/src/chatgpt_token.rs`
  - 從 auth manager 載入 token 到 process memory

- `codex-rs/chatgpt/src/connectors.rs`
  - connectors / apps 相關
  - 這是 ChatGPT ecosystem API 的例子，但不是 thread/turn conversation host

### E. Claude 相關，但不是對話橋接

repo 內有 `.claude` 相關程式碼，但它處理的是設定遷移，不是讓 ClaudeCode 使用 Codex 對話 backend。

- `codex-rs/app-server/src/config/external_agent_config.rs`
  - 掃描 `.claude/settings.json`
  - 匯入 skills / AGENTS.md / plugins / config

- `codex-rs/app-server/src/external_agent_config_api.rs`
  - 對外提供 detect/import API

這兩個檔案很重要，但只對「讀取或遷移 Claude 生態設定」有用，和 conversation backend 植入是兩條不同路。

## 關鍵觀察

### 1. `backend-client` 不是多輪對話整合的首選入口

很多人會先看到 `backend-client`、`chatgpt` crate，就直覺認為那是接 conversation 的地方。實際上，這些 crate 確實會用到 Codex / ChatGPT backend，但真正的 thread / turn / tool / approval / event stream 封裝是在：

- `app-server-protocol`
- `app-server`
- `core`

如果你要的是「ClaudeCode 某個模式像 Codex 一樣對話」，你應該優先整合 app-server，不是直接重做 backend client。

### 2. 不要假設 token 一定落在 `auth.json`

`login/src/auth/storage.rs` 已經說明 storage backend 可以是：

- file
- keyring
- auto
- ephemeral

所以如果你打算「直接讀 `~/.codex/auth.json` 取得 token」，這只在某些配置下成立。若使用 keyring，檔案可能不存在，或只是 fallback。

### 3. 外部 token 注入是可用的，但它是 internal / unstable

`LoginAccountParams::ChatgptAuthTokens` 的註解已經明寫：

- unstable
- internal use only

所以這條路能用，但你要接受 upstream 可能改名、改結構、改 refresh 流程。

### 4. 如果只想在某個 mode 啟用 Codex，最小侵入點是「mode-to-provider bridge」

不要在 ClaudeCode 原本所有對話流程裡散落條件判斷。比較好的做法是：

- 在 mode 切換時決定 provider
- provider 若是 `codex-app-server`
  - 啟動或重用 app-server process
  - 管理一個 `threadId`
  - 把 ClaudeCode 的輸入轉成 `turn/start`
  - 把 Codex notifications 映射回 ClaudeCode UI state

## 推薦整合方案

## 方案 A：用 `codex app-server` 當 ClaudeCode 的對話後端

這是最推薦的方案。

### 適用情境

- 你要的是「在 ClaudeCode 中增加一個 Codex mode」
- 你要 thread / turn / streaming / tool / approval / auth refresh 一起工作
- 你不想重寫 Codex 的 conversation runtime

### ClaudeCode 端應該做什麼

1. mode 切換時決定 provider
2. provider = `codex-app-server` 時，啟動 `codex app-server --listen stdio://`
3. 送 `initialize`
4. 做登入
5. 建 thread
6. 送 turn
7. 讀通知流並更新 UI

### 推薦的客戶端流程

#### A1. 讓 Codex 自己處理 OAuth

如果你不需要 ClaudeCode 自己控整個 OAuth UI，這是最穩的方式。

流程如下：

1. `initialize`
2. `account/login/start` with `{"type":"chatgpt"}`
3. 取得 `auth_url`
4. ClaudeCode 打開瀏覽器或顯示給使用者
5. 等待 `account/login/completed`
6. 收到 `account/updated`
7. `thread/start`
8. `turn/start`

這樣 OAuth callback server、code exchange、token persistence 都由 Codex 接手。

#### A2. ClaudeCode 自己拿 token，再注入 app-server

如果你一定要自己掌握 OAuth UI，則用：

`account/login/start` with `{"type":"chatgptAuthTokens", ...}`

但要額外做一件事：

- 實作 `account/chatgptAuthTokens/refresh` 的回應邏輯

因為 app-server 在 access token 失效時，會透過 `ExternalAuthRefreshBridge` 反向向 client 要新 token。

### 最小 JSON-RPC 互動序列

#### 初始化

```json
{"id":"1","method":"initialize","params":{
  "clientInfo":{"name":"claudecode-codex-mode","title":"ClaudeCode Codex Mode","version":"0.1.0"},
  "capabilities":{"experimentalApi":true}
}}
```

接著：

```json
{"method":"initialized","params":{}}
```

#### 啟動 Codex 管理的 OAuth

```json
{"id":"2","method":"account/login/start","params":{"type":"chatgpt"}}
```

或 device code：

```json
{"id":"2","method":"account/login/start","params":{"type":"chatgptDeviceCode"}}
```

#### 外部 token 注入

```json
{"id":"2","method":"account/login/start","params":{
  "type":"chatgptAuthTokens",
  "accessToken":"<jwt>",
  "chatgptAccountId":"<workspace-id>",
  "chatgptPlanType":"pro"
}}
```

#### 建 thread

```json
{"id":"3","method":"thread/start","params":{
  "cwd":"C:/your/project",
  "model":"gpt-5.4",
  "serviceName":"claudecode-codex-mode"
}}
```

#### 開始 turn

```json
{"id":"4","method":"turn/start","params":{
  "threadId":"<thread-id>",
  "input":[
    {"type":"text","text":"請幫我分析這個 repo 的登入流程"}
  ]
}}
```

#### 你至少要處理的通知

- `thread/started`
- `turn/started`
- `item/agentMessage/delta`
- `rawResponseItem/completed`
- `turn/completed`
- `error`
- `account/login/completed`
- `account/updated`

如果你只想先把「可對話」打通，最低限度先處理：

- `item/agentMessage/delta`
- `turn/completed`
- `error`

### ClaudeCode 端建議抽象

可以切成四個模組：

- `CodexAppServerProcess`
  - 啟動 / 關閉 app-server
  - stdio JSON-RPC framing

- `CodexAuthBridge`
  - `ensureLoggedIn()`
  - `startBrowserLogin()`
  - `startDeviceCodeLogin()`
  - `handleExternalRefreshRequest()`

- `CodexThreadBridge`
  - `ensureThread()`
  - `startTurn()`
  - `interruptTurn()`

- `CodexEventMapper`
  - 把 `item/agentMessage/delta` 映射成 ClaudeCode UI chunk
  - 把 `turn/completed` 映射成 completed state
  - 把 `error` 映射成 ClaudeCode 的錯誤面板

### 方案 A 的推薦變體：`app-server` + companion MCP server

如果你的目標不只是「讓 ClaudeCode 能用 Codex 對話」，而是還要：

- 監控對話品質
- 糾舉 AI 偷懶、漏做驗證、漏收尾
- 參與 plan 討論
- 在必要時提示「這個回合不夠完整，應該補哪些步驟」

那我建議你把系統拆成兩層，而不是把所有責任都放進同一個 bridge：

1. 主執行層：`codex app-server`
2. 監督層：伴隨 ClaudeCode 啟動的 companion MCP server

也就是說：

- `codex app-server` 負責真正的對話 thread / turn / auth / backend
- companion MCP server 負責審核、提示、補 plan、完成度檢查

這樣分工會比「讓 MCP server 直接承接整個對話」穩很多。

### 為什麼 MCP server 適合做監督層，而不是主對話層

如果你把 MCP server 當主對話層，會很快遇到兩個問題：

1. MCP 工具呼叫本質上通常是被動的
2. 你要的是持續監控，而不是模型偶爾想起來才調工具

換句話說，MCP 很適合提供：

- `review_current_plan`
- `check_completion_evidence`
- `flag_missing_verification`
- `propose_recovery_prompt`

但 MCP 本身不天然等於「一定看得到每個回合的所有上下文與工具行為」。

所以如果你要的是真正的 watchdog，而不是 advisory helper，不能只靠「模型願意時才呼叫 MCP tool」。

### 最推薦的架構

建議你把 ClaudeCode 的 Codex mode 拆成三個平面：

1. 對話平面
   - ClaudeCode <-> `codex app-server`
   - 負責 thread / turn / event stream / auth

2. 監督平面
   - ClaudeCode <-> companion MCP server
   - 負責審核、糾偏、completion audit、plan critique

3. 事件平面
   - 由 ClaudeCode host 或 hook 機制，把「使用者輸入 / assistant 回覆 / tool call / tool result / turn 完成」餵給 companion
   - 這一層是 watchdog 能否真的「持續監控」的關鍵

### 這裡最重要的判斷

如果沒有事件平面，只有 MCP server，是不夠的。

因為那代表：

- companion 看不到完整上下文
- companion 只能在被呼叫時才介入
- 你要的「隨時監控」會退化成「偶爾請它 review 一下」

所以正確方向不是：

- `ClaudeCode -> MCP -> Codex backend`

而是：

- `ClaudeCode -> codex app-server` 作主對話
- `ClaudeCode -> companion MCP` 作監督
- `ClaudeCode host/hook -> companion MCP` 作事件餵送

## 針對你提的需求，我會怎麼拆

### 需求 1：能隨時監控對話

這件事不要只靠 MCP 工具呼叫。

比較穩的做法是：

- 每次使用者送出 prompt 時，host 把最新 transcript 摘要送給 companion
- 每次 assistant 完成一段輸出時，再送一次
- 每次 tool call 開始/完成時，都送事件
- 每次 turn completed 時，送 completion snapshot

如果 ClaudeCode 本身有 hook / event callback 可接，優先走那條。

如果沒有，就在你的 ClaudeCode mode bridge 內自己做 event tap。

### 需求 2：糾舉偷懶、漏做、錯誤

這類需求很適合 companion MCP server，但要有明確的審核規則，不要只靠自由聊天。

建議至少做這幾類檢查：

- 計畫缺口
  - 使用者要求了多個子目標，但當前回合只處理了一部分

- 驗證缺口
  - 宣稱完成，但沒有測試、沒有 build、沒有 log、沒有 evidence

- 執行缺口
  - 說會修改檔案，但實際沒有寫入
  - 說會檢查 repo，但實際沒讀關鍵檔案

- 偷懶模式
  - 過早下結論
  - 只給建議、不落地執行
  - 重複輸出泛泛步驟，沒有對應目前 repo 狀態

- 風險模式
  - 高風險變更沒有確認
  - 直接跳過失敗的測試或錯誤
  - 把未知當成已知

### 需求 3：參與計畫討論

這件事也很適合 companion，但建議它扮演的是：

- reviewer
- planner critic
- completion auditor

不是第二個會同時主動改檔的 agent。

比較好的模式是：

- 主 agent 負責執行
- companion 負責指出：
  - plan 是否漏步
  - step 順序是否不合理
  - 哪些驗證應該補
  - 哪些風險還沒處理

這樣兩者職責清楚，不容易互相打架。

## companion MCP server 的建議工具面

如果你要做這個 sidecar MCP，我建議先只做讀取 / 審核型工具，不要一開始就給它寫入能力。

第一版工具可以是：

- `review_transcript_segment`
  - 輸入最近 N 則 user / assistant / tool 事件
  - 輸出風險、漏項、建議追問

- `review_current_plan`
  - 輸入目前任務目標與暫定 plan
  - 輸出缺漏步驟與順序建議

- `check_completion_evidence`
  - 輸入 assistant 宣稱完成的內容、工具證據、測試結果
  - 判斷是否可接受為完成

- `flag_laziness_or_shortcut`
  - 偵測是否有未驗證結論、過早收尾、只說不做

- `propose_recovery_prompt`
  - 當發現缺漏時，產出一段適合注入下一回合的糾偏 prompt

- `summarize_open_risks`
  - 持續維護未解決風險列表

第二版才考慮：

- `start_codex_side_review`
  - companion 自己開一條獨立的 Codex review thread 做旁路審查

但這條建議先不要做成預設，因為成本與複雜度會明顯上升。

## companion 應該怎麼介入

你要先決定 companion 是 advisory 還是 gatekeeper。

### Advisory 模式

最容易落地，建議先做這個。

行為：

- companion 產出警告與建議
- ClaudeCode UI 顯示提醒
- 主 agent 可根據提醒修正

適合先驗證效果。

### Soft gate 模式

適合第二階段。

行為：

- 若 companion 判定「高風險漏項」，在下一回合前自動插入一段糾偏 instruction
- 例如：
  - 先補測試
  - 先讀某些檔案
  - 先驗證 build

這比 advisory 強，但仍然不硬性阻塞。

### Hard gate 模式

只有在你真的掌握 host 流程時才建議。

行為：

- 若沒有 completion evidence，不允許宣告完成
- 若高風險變更未驗證，不允許進入 finalize

這類機制有效，但很容易讓使用體驗變差，所以不要在第一版就上。

## 我對這個方向的具體建議

### 建議 1：不要把 companion MCP server 當成唯一監控來源

MCP server 應該是監督邏輯承載點，不是事件真相來源。

事件真相來源應該是：

- ClaudeCode host 自己的 transcript / tool events
- 或你的 Codex mode bridge 收到的 thread/turn notifications

companion 只負責分析。

### 建議 2：先做 read-only watchdog，不要先給 sidecar 寫檔

如果 companion 一開始就有寫檔能力，你很快會碰到：

- 主 agent 和 companion 同時改同一份檔案
- 監督 agent 變成第二個執行 agent
- 問題從「監控」變成「多 agent 寫入協調」

第一版應該明確限制：

- companion 不改 workspace
- companion 不直接下 destructive command
- companion 只出 review / warning / plan critique / recovery prompt

### 建議 3：主 thread 和監督 thread 分離

如果你讓 companion 也走 Codex backend，我建議至少分兩條 thread：

- work thread
  - 真正處理使用者任務

- oversight thread
  - 專門拿來做 review / plan critique / completion audit

不要把監督訊息和主工作訊息全混在同一 thread，否則上下文會很快膨脹，也容易污染主 agent 行為。

### 建議 4：糾偏訊息要標準化

companion 不要只輸出自然語言長文評論。

最好輸出結構化結果，例如：

- `severity`
- `category`
- `evidence`
- `recommended_action`
- `should_block_completion`

這樣 ClaudeCode host 才能決定：

- 只顯示提醒
- 自動插一段 corrective prompt
- 阻止 finalize

### 建議 5：先做 turn-complete audit，再做 mid-turn watchdog

第一版先不要追求「每一秒都監控」。

最划算的順序是：

1. turn completed 後做 audit
2. assistant 準備 finalize 前做 completion check
3. tool completed 後做 evidence check
4. 最後才做 mid-turn 即時監控

這樣可以先把價值最高、整合最穩的部分做起來。

## 實作上我會怎麼排優先順序

### Phase 1

- ClaudeCode 的 Codex mode 主對話先走 `codex app-server`
- 新增 companion MCP server，但先只做：
  - `review_current_plan`
  - `check_completion_evidence`
  - `propose_recovery_prompt`
- companion 只在 turn completed 後被呼叫

### Phase 2

- 加入 tool 事件餵送
- 對 shell / apply-patch / MCP tools 做 evidence 審核
- 加入「宣稱完成前必做一次 audit」

### Phase 3

- 加入 soft gate
- 高風險缺漏時，自動插入 corrective instruction
- companion 維護持續中的 risk register

### Phase 4

- 若效果真的值得，再考慮讓 companion 自己開獨立 Codex oversight thread

## 基於已確認前提的 Phase 1 規格

這一節是依照你目前已確認的兩個前提收斂後的實作規格，目標是讓 Claude Code 可以直接拿去拆工。

### 已確認前提

- `event tap` 可穩定取得 `user` / `tool` / `turn` 事件
- `assistant response body` 目前拿不到，Phase 1 不以它為依賴
- Phase 1 先用 `tool` 呼叫軌跡、turn 狀態與 completion evidence 做間接判斷
- 預設介入模式是 `advisory`
- 兩個 `soft gate checkpoint` 是：
  - completion declaration
  - architecture decisions
- 不做 `hard gate`

### Phase 1 目標

- 在不依賴 assistant body 的前提下，找出明顯的偷懶、缺步、驗證不足與完成宣稱不實
- 讓 companion 可以對當前 plan 給出 critique，並在適當時機要求補強
- 在不阻塞主流程的情況下，把 corrective guidance 回注到下一輪 Codex 對話
- 讓整體架構從第一版就保留未來接 assistant streaming tap 或 conversation state API 的擴充空間

### Phase 1 非目標

- 不做逐 token 或 mid-turn 即時監控
- 不做 hard block，也不在高風險時直接鎖死流程
- 不要求 companion 直接讀取 assistant natural-language response body
- 不讓 companion 直接修改 workspace 或自行執行修復
- 不把 companion 做成第二個長駐寫入 agent

### Phase 1 的事件與資料模型

Phase 1 應只依賴目前可穩定取得的事件，不假設 assistant message delta 一定能被 Claude Code host 拿到。

可依賴的事件來源：

- `user prompt submitted`
- `tool start`
- `tool completed`
- `turn started`
- `turn completed`
- `completion declaration`
- `architecture decision requested`

這與 Codex 現有 protocol / hook 能力是對得上的：

- hook event 名稱可見於 [v2.rs](/C:/CodexSource/codex/codex-rs/app-server-protocol/src/protocol/v2.rs:439)
- managed hook 設定欄位可見於 [v2.rs](/C:/CodexSource/codex/codex-rs/app-server-protocol/src/protocol/v2.rs:966)
- 對話建立與送 turn 的主入口在 [v2.rs](/C:/CodexSource/codex/codex-rs/app-server-protocol/src/protocol/v2.rs:3259) 與 [v2.rs](/C:/CodexSource/codex/codex-rs/app-server-protocol/src/protocol/v2.rs:4939)
- `thread/started`、`turn/started` 通知在 [v2.rs](/C:/CodexSource/codex/codex-rs/app-server-protocol/src/protocol/v2.rs:6096) 與 [v2.rs](/C:/CodexSource/codex/codex-rs/app-server-protocol/src/protocol/v2.rs:6151)
- raw events 與 agent delta 雖然存在，但目前不應當成 Phase 1 必要依賴，因為 `experimentalRawEvents` 被標成 internal，相關定義可見於 [v2.rs](/C:/CodexSource/codex/codex-rs/app-server-protocol/src/protocol/v2.rs:3311)、[v2.rs](/C:/CodexSource/codex/codex-rs/app-server-protocol/src/protocol/v2.rs:6322) 與 [v2.rs](/C:/CodexSource/codex/codex-rs/app-server-protocol/src/protocol/v2.rs:6332)

Phase 1 建議的最小 state：

- `thread_id`
- `turn_id`
- `user_goal_summary`
- `current_plan_snapshot`
- `tool_trace[]`
- `evidence[]`
- `declared_completion`
- `pending_architecture_decision`
- `open_risks[]`
- `last_companion_assessment`

### Phase 1 的核心判斷方式

因為沒有 assistant body，Phase 1 的 companion 不能做「語義級逐句審查」，要改成做「行為證據審查」。

推薦的判斷策略如下：

- `plan critique`
  - 依 user prompt、最近幾輪工具軌跡與目前 plan snapshot，判斷是否缺少明顯步驟
- `laziness / shortcut detection`
  - 宣稱完成，但幾乎沒有對應工具證據
  - 該跑測試卻未跑測試
  - 該讀檔卻直接下結論
  - 做了修改卻沒有驗證
- `completion evidence check`
  - 將完成宣稱與已收集 evidence 對照，產出缺口
- `architecture decision review`
  - 在採納新架構、新 bridge、新 state model 前，檢查是否評估了替代方案、耦合面與回退策略

換句話說，Phase 1 不是在問「assistant 說得好不好」，而是在問「assistant 到底有沒有做出足夠證據支持它現在要說的話」。

### Phase 1 的介入策略

Phase 1 採 `hybrid` 模式，但要明確分成三種介入強度。

#### 1. 預設模式：advisory

當 companion 發現風險，但尚未碰到 checkpoint 時：

- 不中斷主流程
- 產出結構化 warning / critique
- 由 Claude Code host 在下一次送 `turn/start` 前，將 corrective guidance 注入為 host-side `additionalContext`

這裡的 `additionalContext` 應視為 Claude Code host 自己的注入機制，不要把它寫成 Codex protocol 既有欄位。

#### 2. Soft gate A：completion declaration

當主 agent 宣稱「已完成」或進入 finalize 前：

- host 必須先呼叫 companion 的 completion audit
- 若 companion 回報高風險缺口，host 不直接 finalize
- host 轉而注入 corrective prompt，要求主 agent 補齊驗證、補充證據或承認未完成

這仍然不是 hard gate，因為流程是「要求再做一輪」，不是「永久拒絕前進」。

#### 3. Soft gate B：architecture decisions

當主 agent 準備做以下高影響決策時，必須先跑 architecture review：

- 新增 bridge / adapter / provider abstraction
- 修改 thread / state ownership
- 引入新的長駐 sidecar 或 background process
- 新增 auth token lifecycle 或 refresh path

若 companion 判定設計論證不足：

- host 不直接接受該決策作為既定方案
- 先把 critique 注回主 agent
- 要求主 agent 補 alternatives、tradeoffs、failure modes、rollback plan

### Phase 1 companion 的建議輸出格式

companion 不應只回一段長文，而應固定輸出可機器處理的結構。

最低限度建議包含：

- `status`
  - `ok`
  - `warning`
  - `needs_followup`
- `severity`
  - `low`
  - `medium`
  - `high`
- `category`
  - `plan_gap`
  - `missing_evidence`
  - `laziness_or_shortcut`
  - `architecture_risk`
  - `completion_risk`
- `summary`
- `evidence`
- `recommended_action`
- `should_inject_additional_context`
- `requires_soft_gate_retry`

### Phase 1 companion MCP tool 面

第一版不需要太多工具，否則 host orchestration 會先變複雜。

建議只做四個：

- `review_current_plan`
  - 輸入：目前 user goal、plan snapshot、最近幾輪 tool trace
  - 輸出：是否缺步、是否有不合理 shortcut、下一步建議
- `check_completion_evidence`
  - 輸入：本輪與最近相關輪次的 evidence、修改摘要、驗證摘要
  - 輸出：是否足以支持完成宣稱
- `review_architecture_decision`
  - 輸入：候選方案、影響面、替代方案、風險
  - 輸出：是否可接受、缺漏點、建議補充項
- `propose_recovery_prompt`
  - 輸入：上一個 tool 的 assessment 結果
  - 輸出：給主 agent 的短 corrective prompt

`flag_laziness_or_shortcut` 可以先內嵌在 `review_current_plan` 與 `check_completion_evidence` 內，不必獨立成第五個工具。

### Phase 1 的元件責任切分

#### Claude Code host / mode bridge

- 啟動與管理 `codex app-server`
- 啟動 companion MCP server
- 維護 `thread_id` 與 mode state
- 收集 user / tool / turn 事件
- 在 checkpoint 時決定要不要呼叫 companion
- 將 advisory 結果轉成下一輪的 host-side `additionalContext`

#### Companion MCP server

- 維護輕量 oversight state
- 執行 plan critique / completion audit / architecture review
- 產出結構化 assessment
- 不直接寫檔
- 不直接修改主對話 thread

#### Codex app-server

- 仍是主對話 transport 與 thread/turn lifecycle 的唯一來源
- 負責 OAuth / account login / thread start / turn start / event stream

### Phase 1 任務拆解

#### Task 1：定義事件契約

- 定義 host 送給 companion 的事件 schema
- 至少包含：
  - `session_id`
  - `thread_id`
  - `turn_id`
  - `event_type`
  - `timestamp`
  - `payload`
- `payload` 需支援：
  - user prompt summary
  - tool name
  - tool args summary
  - tool result summary
  - completion declaration marker
  - architecture decision marker

完成標準：

- host 與 companion 可在不依賴 assistant body 的前提下，用同一份 schema 溝通

#### Task 2：建立 oversight state store

- 實作 companion 端的 in-memory state store
- 以 `thread_id` + `turn_id` 做索引
- 能追蹤：
  - 目前 plan
  - 最近工具序列
  - 已知 evidence
  - 未結風險

完成標準：

- 任一 checkpoint 到來時，companion 能從 state store 還原出可審核上下文

#### Task 3：完成 advisory path

- host 在一般 turn completed 後呼叫 `review_current_plan`
- 若結果是 `warning` 或 `needs_followup`：
  - 呼叫 `propose_recovery_prompt`
  - 將結果注入下一輪 `additionalContext`

完成標準：

- 不需要人工介入，系統即可在下一輪把 critique 回灌給主 agent

#### Task 4：完成 soft gate A

- 定義 completion declaration 的觸發時機
- 觸發時一定呼叫 `check_completion_evidence`
- 若 evidence 不足：
  - 不 finalize
  - 將 corrective prompt 注入下一輪
  - 要求主 agent 補驗證或改寫結論

完成標準：

- 主 agent 不能在明顯缺證據時直接收斂成最終完成

#### Task 5：完成 soft gate B

- 在 host 端定義 architecture decision 的觸發來源
- 觸發時呼叫 `review_architecture_decision`
- 若評估不足：
  - 把 critique 回注主 agent
  - 要求補 alternatives / tradeoffs / rollback

完成標準：

- 高影響設計選擇至少會被 companion 審過一次

#### Task 6：定義 heuristics 與風險規則

- 先不要做複雜模型判斷，先做明確規則
- 例如：
  - 有改檔但沒有測試或執行驗證
  - 宣稱完成但工具活動極少
  - 連續兩輪都在說要做，卻沒有相應 evidence
  - architecture 決策沒有 alternatives 與 failure mode

完成標準：

- companion 的輸出不依賴模糊直覺，而有可追蹤的規則基礎

#### Task 7：定義 override 與降級行為

- soft gate 觸發時，host 如何提示使用者或主 agent
- companion 當機時，是否降級回 advisory-only
- event tap 缺漏時，是否直接標記 `insufficient_visibility`

完成標準：

- 系統不會因 companion 當機就卡死主對話

### Phase 1 驗收標準

- 使用者可進入 Claude Code 的 Codex mode，主對話仍正常走 `codex app-server`
- companion 可在沒有 assistant body 的情況下持續收到 user / tool / turn 事件
- 一般情況下 companion 只做 advisory，不打斷主流程
- completion declaration 時一定觸發 completion audit
- architecture decision 時一定觸發 architecture review
- 高風險缺漏時，系統會把 critique 注入下一輪，而不是直接放行 finalize
- companion 不直接改檔、不直接執行修復、不成為第二個寫入 agent

### Phase 1 之後再做的事

等 Phase 1 穩定後，再考慮：

- assistant response body 的 JSONL streaming tap
- conversation state API
- tool-level 即時 evidence scoring
- persistent risk register
- 獨立的 Codex oversight thread
- 更細的 soft gate policy

這個順序比較合理，因為它把第一版焦點放在「是否有足夠證據支持當前結論」，而不是過早追求全文語義監控。

## 對你這個方向的總結判斷

你的方向是合理的，但我會把它重新描述成：

- 主對話 backend：`codex app-server`
- 伴隨啟動的 companion MCP server：監督層
- host/hook/event tap：保證 companion 有持續監控所需的資料

不要把它做成：

- 「MCP server 同時承接對話 + 監督 + 寫入」

那樣很容易耦合失控。

## 如果你要把這條路正式交給 Claude Code 實作，先定義這三件事

1. companion 能看到哪些事件
   - 只有 turn completed？
   - 還是 user prompt / assistant delta / tool start / tool end 都看得到？

2. companion 的介入力度
   - advisory
   - soft gate
   - hard gate

3. companion 是否有寫入權限
   - 建議第一版一律沒有

## 方案 B：直接在你的程式內重用 `codex_login` + `core`

這適合你本身就是 Rust 宿主，而且想做深度嵌入。

### 你會碰到的檔案

- `codex-rs/login/src/*`
- `codex-rs/core/src/thread_manager.rs`
- `codex-rs/core/src/codex_thread.rs`
- `codex-rs/core/src/session/*`

### 優點

- 少一層 app-server process
- 可以直接進到最核心 thread 物件

### 缺點

- 耦合非常深
- 你要自己處理更多 runtime wiring
- 之後同步 upstream 變更的成本高

除非 ClaudeCode 本身就是 Rust 程式而且你願意直接 vendor / fork 這些 crate，不然不建議從這層開始。

## 方案 C：直接打 backend REST

這是最不推薦的。

### 原因

- 你會繞過 thread/turn 的高階抽象
- 你要自己補：
  - OAuth token refresh
  - workspace/account header
  - event stream handling
  - tool / approval / mode / thread state
- repo 內的 `backend-client` 也不是為了「直接提供完整 conversation host API」而設計

### 什麼情況下才值得

- 你只想調少數 backend endpoint
- 例如 rate limits、task details、connectors、config requirements

如果目標是可對話模式，不要從這條開始。

## 植入 ClaudeCode 的實作建議

### 1. 在 mode 層做 provider 切換

建議不要把 Codex logic 塞進 ClaudeCode 原本的 provider 分支內到處判斷。

比較乾淨的做法：

- `default mode` 走原本 Claude provider
- `codex mode` 走 `CodexBridge`

這樣之後你要擴充：

- login status 顯示
- workspace 切換
- thread resume
- 中斷與重試

都不會污染原始 provider。

### 2. thread id 由 ClaudeCode mode state 持有

當使用者切到 Codex mode 時：

- 若 mode state 沒有 thread id，就先 `thread/start`
- 若 mode state 已有 thread id，就直接 `turn/start`

不要每次都開新 thread，否則會失去多輪上下文。

### 3. 若使用外部 token 注入，務必實作 refresh callback

這是很多人最容易漏掉的一點。

當 app-server 使用 `chatgptAuthTokens` 模式時，token 失效後不會自己去 refresh OAuth session；它會送一個 server request 給 client：

- `account/chatgptAuthTokens/refresh`

ClaudeCode 必須回：

- 新的 `accessToken`
- `chatgptAccountId`
- `chatgptPlanType`

如果這段沒做，初期看起來能跑，但 token 一過期就整條鏈斷掉。

### 4. 如果你想共用 Codex 的 auth storage，不要只讀檔

因為 storage mode 可能是 keyring / auto。

如果你真的要讀既有登入狀態，比較正確的方式是：

- 走 `AuthManager`
- 或乾脆把 login 也交給 app-server

### 5. `.claude` 遷移功能不要和 conversation bridge 混在一起

`external_agent_config` 那條線只是在處理：

- `.claude/settings.json`
- `.claude/skills`
- `CLAUDE.md`
- plugins / marketplace 導入

這些可以幫你把 Claude 生態設定搬進 Codex，但不會自動讓 ClaudeCode 能用 Codex thread/turn。

## 建議的開發順序

1. 先做 `codex app-server` 的 stdio JSON-RPC bridge
2. 先只支援 `account/login/start -> chatgpt`
3. 再打通 `thread/start`
4. 再打通 `turn/start`
5. 先只處理 `item/agentMessage/delta` 與 `turn/completed`
6. 確認 mode state 能保留 `threadId`
7. 再決定是否需要外部 token 注入
8. 若需要外部 token 注入，再補 `account/chatgptAuthTokens/refresh`

這樣能最快把「可對話」做出來，而且不會一開始就卡在 refresh / storage / workspace edge cases。

## 你最需要先看的檔案

如果你現在就要開始做，我建議閱讀順序是：

1. `codex-rs/app-server/src/codex_message_processor.rs`
2. `codex-rs/app-server-protocol/src/protocol/v2.rs`
3. `codex-rs/app-server-protocol/src/protocol/common.rs`
4. `codex-rs/login/src/server.rs`
5. `codex-rs/login/src/device_code_auth.rs`
6. `codex-rs/login/src/auth/manager.rs`
7. `codex-rs/login/src/auth/storage.rs`
8. `codex-rs/core/src/thread_manager.rs`
9. `codex-rs/core/src/codex_thread.rs`
10. `codex-rs/core/src/session/mod.rs`

## 對你的需求的直接建議

如果你要的是「ClaudeCode 特定模式可直接和 Codex 對話，而且走 OAuth 與 Codex 既有 backend」，我建議你做成：

- ClaudeCode mode
  - 啟動 `codex app-server`
  - 用 `account/login/start` 做登入
  - 用 `thread/start` / `turn/start` 做對話
  - 用通知流回填 UI

如果你有強需求要讓 ClaudeCode 自己接管 OAuth token，再把 token 灌給 Codex，就改成：

- ClaudeCode mode
  - 啟動 `codex app-server`
  - `account/login/start(type=chatgptAuthTokens)`
  - 實作 `account/chatgptAuthTokens/refresh`
  - 其餘 thread/turn 流程不變

## 補充：repo 內可直接參考的 client 端實作

如果你想看一個外部 client 怎麼用 app-server，先看：

- `sdk/python/src/codex_app_server/client.py`
- `sdk/python/src/codex_app_server/async_client.py`
- `sdk/typescript/src/thread.ts`

這些雖然不是 ClaudeCode 專用，但很適合拿來對照：

- thread 如何建立
- turn 如何開始
- streaming event 如何消費

## 不建議的做法

- 直接讀 `auth.json` 然後自己打 backend
- 把 `.claude` 遷移功能誤認成 conversation bridge
- 在 ClaudeCode 所有 mode 裡都摻雜 Codex provider 分支
- 使用 `chatgptAuthTokens` 卻不實作 refresh callback

## 最後的判斷

從 repo 現況來看，Codex 已經有完整的：

- OAuth / device code login
- token storage / refresh
- thread / turn abstraction
- app-server protocol
- SDK client 參考實作

所以你真正要做的，不是「再做一套 Codex 對話引擎」，而是「在 ClaudeCode mode 層接上一個 Codex bridge」。

這樣改動最小，也最符合 upstream 現有設計。
