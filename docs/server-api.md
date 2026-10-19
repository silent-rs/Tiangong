# 天工 Server API 对接文档

## 概述

天工 Server 提供 HTTP API，支持外部 Bot / Connector 接入，实现消息收发、会话管理、Webhook 等能力。

- **基础路径**: `/api/v1`
- **默认监听**: `127.0.0.1:8080`
- **认证方式**: Bearer Token（通过 `Authorization` 请求头）
- **数据格式**: JSON

## 快速开始

### 启动 Server

```bash
tiangong server --host 127.0.0.1 --port 8080 --token your_secret_token
```

| 参数 | 说明 | 默认值 |
|------|------|--------|
| `--host` | 监听地址 | `127.0.0.1` |
| `--port` | 监听端口 | `8080` |
| `--token` | API 认证 Token | 无（不鉴权） |
| `-d` | 后台守护进程运行 | 否 |

### 认证

所有接口（除健康检查外）均需在请求头中携带 Token：

```
Authorization: Bearer your_secret_token
```

---

## HTTP API

### 健康检查

```
GET /api/v1/health
```

**无需认证**

**响应**：

```json
{ "status": "ok" }
```

### 发送消息（简易）

```
POST /api/v1/chat
```

直接发送文本消息并同步等待 AI 回复。省略 `session_id` 时会发起新对话；继续已有
对话时传入上一次响应中的 `session_id`。

**请求体**：

```json
{
  "session_id": "可选，指定会话 ID",
  "message": "用户消息文本"
}
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| session_id | string | 否 | 省略时发起新对话；传入时继续对应会话 |
| message | string | 是 | 用户消息内容 |

**响应**：

```json
{
  "session_id": "会话 ID",
  "response": "AI 回复文本"
}
```

### 发送消息（Connector 完整接口）

```
POST /api/v1/messages
```

外部 Bot / Connector 统一消息入口，支持多模态内容和媒体资源。

**请求体**：

```json
{
  "connector": "feishu-bot",
  "channel_id": "oc_xxxxxx",
  "message_id": "可选，外部消息 ID",
  "message": "文本消息，只发附件时可为空",
  "media": [
    {
      "kind": "image",
      "url": "https://example.com/photo.jpg",
      "mime_type": "image/jpeg",
      "title": "图片描述",
      "capability": "multimodal"
    }
  ]
}
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| connector | string | 否 | Connector 名称，默认 `external-bot` |
| channel_id | string | 是 | 外部通道 ID（如飞书 chat_id） |
| message_id | string | 否 | 外部消息 ID，不传时自动生成 |
| message | string | 否 | 用户正文，只发附件时可省略或为空 |
| media | array | 否 | 全部附件（图片、视频、音频、文件），按发送顺序排列 |

> `message` 与 `media` 不能同时为空。只发图片时 `message` 留空即可，Server 不会代拟占位文本。
>
> **已废弃**：旧的 `content` 字段（单一类型正文，格式见下方「回复内容类型」）仍被接受，
> Server 在入口转为 `message` + `media`：附件排在 `media` 之前，图片/视频的 `caption`
> 仅在 `message` 为空时作为正文。新接入请勿使用，确认无外部调用方后将删除。

**响应**：

```json
{
  "session_id": "会话 ID",
  "connector": "feishu-bot",
  "channel_id": "oc_xxxxxx",
  "reply_to": "回复的消息 ID",
  "message": "AI 回复文本",
  "content": { "type": "text", "text": "AI 回复文本" },
  "attachments": [
    { "type": "image", "url": "/path/to/generated-image.png", "caption": null }
  ]
}
```

`content` 是回复的首项，`attachments` 是同一回复中的其余附件；没有附件时省略
`attachments`。回复引用天工媒体目录或当前会话工作区内的本地文件时，Server 会
移除正文中的本机路径，并把文件放入上述结构化字段。

### 回复内容类型

响应中的 `content` 与 `attachments` 元素支持以下类型：

#### 文本消息

```json
{ "type": "text", "text": "消息内容" }
```

#### 图片消息

```json
{ "type": "image", "url": "https://example.com/photo.jpg", "caption": "图片描述" }
```

#### 音频消息

```json
{ "type": "audio", "url": "https://example.com/voice.ogg", "duration": 30 }
```

#### 视频消息

```json
{ "type": "video", "url": "https://example.com/clip.mp4", "caption": "视频描述" }
```

#### 文件消息

```json
{ "type": "file", "url": "https://example.com/doc.pdf", "name": "文档.pdf" }
```

### 媒体资源

`media` 数组中的每个元素：

```json
{
  "kind": "image",
  "url": "资源 URL",
  "mime_type": "可选，MIME 类型",
  "title": "可选，标题或描述",
  "capability": "可选，能力标识（如 multimodal）"
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| kind | string | `image`、`video`、`audio`、`file` |
| url | string | 资源 URL（支持 http/https/data URL） |
| mime_type | string | MIME 类型（可选） |
| title | string | 标题或描述（可选） |
| capability | string | 能力标识（可选，如 `multimodal`） |

> **归档**：Server 在消息投递前会把所有图片和文件附件归档到本地 `~/.tiangong/media/`。
> `kind=image` 归档到 `images/`，`kind=file` 归档到 `files/`。文件类型/MIME 只用于
> 确定扩展名，不影响是否归档。无法识别类型的文件以 `application/octet-stream` 保存。

### 会话管理

#### 列出会话

```
GET /api/v1/sessions?limit=20&offset=0
```

**响应**：

```json
[
  {
    "id": "会话 ID",
    "title": "会话标题",
    "message_count": 12,
    "created_at": "2026-05-20T10:00:00",
    "updated_at": "2026-05-20T11:30:00"
  }
]
```

#### 获取会话详情

```
GET /api/v1/sessions/{id}
```

**响应**：

```json
{
  "id": "会话 ID",
  "title": "会话标题",
  "messages": [
    {
      "id": "消息 ID",
      "role": "user",
      "content": "消息内容",
      "created_at": "2026-05-20T10:00:00"
    }
  ],
  "created_at": "2026-05-20T10:00:00",
  "updated_at": "2026-05-20T11:30:00"
}
```

#### 获取会话费用

```
GET /api/v1/sessions/{id}/cost
```

#### 取消会话当前轮次

```
POST /api/v1/sessions/{id}/cancel
```

无需请求体。需要认证，并要求调用方拥有管理会话权限（`can_manage_sessions`）。接口只
尝试向当前活跃执行投递取消命令，不等待整轮收尾；发送请求正处于准备或入队边界时，
接口会短暂等待该边界后再重试一次。

**响应**：

```json
{ "cancelled": true, "id": "会话 ID" }
```

`cancelled: true` 表示取消命令已投递到活跃执行，`false` 表示本次没有可接受取消命令
的活跃执行。`false` 不等同于 HTTP 404，也不表示会话资源不存在。

#### 删除会话

```
DELETE /api/v1/sessions/{id}
```

**响应**：

```json
{ "status": "deleted", "id": "会话 ID" }
```

### 关闭 Server

```
POST /api/v1/server/shutdown
```

优雅关闭 Server。

**响应**：

```json
{ "status": "shutting_down" }
```

### Webhook 管理

Webhook 是 Server 内置的 HTTP 触发能力，独立于定时任务。存储在 `~/.tiangong/webhooks/`。

#### 列出所有 Webhook

```
GET /api/v1/webhooks
```

**响应**：

```json
{
  "total": 1,
  "items": [
    {
      "id": "Webhook ID",
      "name": "Webhook 名称",
      "description": "描述",
      "session_id": "可选，关联会话 ID",
      "payload": "触发时发送给 LLM 的任务描述",
      "secret": "签名密钥（可选）",
      "enabled": true,
      "created_at": "2026-05-25 10:00:00",
      "updated_at": "2026-05-25 10:00:00"
    }
  ]
}
```

#### 创建 Webhook

```
POST /api/v1/webhooks
```

**请求体**：

```json
{
  "name": "GitHub Push 触发",
  "description": "当有新代码推送时触发代码审查",
  "session_id": "可选，关联会话 ID",
  "payload": "请审查最新提交的代码变更",
  "secret": "可选，签名密钥",
  "enabled": true
}
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| name | string | 是 | Webhook 名称 |
| description | string | 是 | 描述 |
| session_id | string | 否 | 关联会话 ID，为空时自动创建新会话 |
| payload | string | 是 | 触发时发送给 LLM 的任务描述 |
| secret | string | 否 | 签名密钥，配置后 invoke 时需验证 |
| enabled | boolean | 否 | 是否启用，默认 `true` |

**响应**（201）：返回创建的 Webhook 对象。

#### 获取 Webhook 详情

```
GET /api/v1/webhooks/{id}
```

**响应**：返回 Webhook 对象。

#### 更新 Webhook

```
PUT /api/v1/webhooks/{id}
```

**请求体**（所有字段可选）：

```json
{
  "name": "更新后的名称",
  "description": "更新后的描述",
  "session_id": "新的关联会话 ID",
  "payload": "更新后的任务描述",
  "secret": "更新后的签名密钥",
  "enabled": true
}
```

**响应**：返回更新后的 Webhook 对象。

#### 删除 Webhook

```
DELETE /api/v1/webhooks/{id}
```

**响应**：

```json
{ "status": "deleted", "id": "Webhook ID" }
```

#### 手动触发 Webhook（需认证）

```
POST /api/v1/webhooks/{id}/trigger
```

通过 API 手动触发 Webhook 执行，需要认证。

**响应**：

```json
{
  "webhook_id": "Webhook ID",
  "session_id": "关联会话 ID",
  "status": "triggered"
}
```

#### 外部触发 Webhook（无需认证）

```
POST /api/v1/webhooks/{id}/invoke
```

供外部系统调用的触发端点，**无需 Bearer Token 认证**。如果 Webhook 配置了 `secret`，需在请求头中传入签名。

**请求头**：

| 请求头 | 说明 |
|--------|------|
| X-Webhook-Signature | 签名验证（配置了 secret 时必填） |

**响应**：

```json
{
  "webhook_id": "Webhook ID",
  "status": "triggered"
}
```

**cURL 示例**：

```bash
# 无签名验证
curl -X POST http://127.0.0.1:8080/api/v1/webhooks/WEBHOOK_ID/invoke

# 有签名验证
curl -X POST http://127.0.0.1:8080/api/v1/webhooks/WEBHOOK_ID/invoke \
  -H "X-Webhook-Signature: your_secret"
```

#### 查询 Webhook 执行历史

```
GET /api/v1/webhooks/{id}/runs?limit=20
```

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| limit | number | 20 | 返回记录数量 |

**响应**：

```json
{
  "total": 3,
  "items": [
    {
      "id": "执行记录 ID",
      "webhook_id": "Webhook ID",
      "session_id": "使用的会话 ID",
      "status": "succeeded",
      "started_at": "2026-05-25 10:00:00",
      "finished_at": "2026-05-25 10:00:15",
      "result_summary": "执行结果摘要..."
    }
  ]
}
```

`status` 取值：`running`、`succeeded`、`failed`

---

## 对接示例

### cURL 示例

**发送文本消息**：

```bash
curl -X POST http://127.0.0.1:8080/api/v1/messages \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer your_secret_token" \
  -d '{
    "connector": "my-bot",
    "channel_id": "channel_001",
    "message": "你好，请帮我分析一下这段代码"
  }'
```

**发送图片消息**：

```bash
curl -X POST http://127.0.0.1:8080/api/v1/messages \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer your_secret_token" \
  -d '{
    "connector": "my-bot",
    "channel_id": "channel_001",
    "message": "请描述这张图片",
    "media": [
      {
        "kind": "image",
        "url": "https://example.com/photo.jpg",
        "capability": "multimodal"
      }
    ]
  }'
```

**发送图文混合消息**：

```bash
curl -X POST http://127.0.0.1:8080/api/v1/messages \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer your_secret_token" \
  -d '{
    "connector": "my-bot",
    "channel_id": "channel_001",
    "message": "请分析这张截图中的错误",
    "media": [
      {
        "kind": "image",
        "url": "data:image/png;base64,iVBOR...",
        "mime_type": "image/png",
        "capability": "multimodal"
      }
    ]
  }'
```

### Python 示例

```python
import requests

BASE_URL = "http://127.0.0.1:8080/api/v1"
TOKEN = "your_secret_token"
HEADERS = {
    "Authorization": f"Bearer {TOKEN}",
    "Content-Type": "application/json",
}

# 发送文本消息
def send_text(channel_id: str, message: str):
    resp = requests.post(
        f"{BASE_URL}/messages",
        headers=HEADERS,
        json={
            "connector": "python-bot",
            "channel_id": channel_id,
            "message": message,
        },
    )
    return resp.json()

# 发送图片消息
def send_image(channel_id: str, text: str, image_url: str):
    resp = requests.post(
        f"{BASE_URL}/messages",
        headers=HEADERS,
        json={
            "connector": "python-bot",
            "channel_id": channel_id,
            "message": text,
            "media": [
                {
                    "kind": "image",
                    "url": image_url,
                    "capability": "multimodal",
                }
            ],
        },
    )
    return resp.json()

# 列出会话
def list_sessions():
    resp = requests.get(f"{BASE_URL}/sessions", headers=HEADERS)
    return resp.json()

# 使用示例
result = send_text("channel_001", "你好")
print(result)
```

---

## 错误处理

所有错误响应格式统一：

```json
{
  "code": 400,
  "msg": "错误描述",
  "data": null
}
```

常见 HTTP 状态码：

| 状态码 | 说明 |
|--------|------|
| 400 | 请求参数错误 |
| 401 | 未认证或 Token 无效 |
| 404 | 资源不存在 |
| 500 | 服务端内部错误 |

---

## 数据流

### 消息处理流程

```
外部 Bot → POST /api/v1/messages
  → 认证校验 + 权限校验
  → 正文与附件统一为 message + media
  → ServerCoreManager.send_connector_message_and_wait
    → TiangongCore.send_message_with_id(text, id, media)
      → React Engine 执行
  → 返回 AI 回复
```

### 媒体资源传递

```
外部 Bot 发送图片:
  → message: 正文（可为空）
  → media: [MediaAsset { kind: Image, url, ... }]
  → CoreManager: send_message_with_id(text, id, media)
  → TiangongCore: 媒体归档到本地 → session 消息包含完整媒体数据
```
