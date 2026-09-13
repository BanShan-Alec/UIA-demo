# uia-probe — Windows UIA 划词提取 PoC（Chromium 专项）

验证通过 **Electron + Rust (napi-rs)** 调用 Windows UI Automation，无剪贴板污染地提取 Chromium 体系（Chrome / Edge / Electron 应用）选中文字及屏幕物理坐标。

## 结构

```
├── src/main/index.ts        # Electron 主进程：全局快捷键 + JSON 日志（走审计接口）
├── src/main/config.ts       # uia-config.json 读写（配置文件 > 环境变量 > 默认值）
├── src/preload/index.ts     # contextBridge：onProbeResult / getState / getConfig / setConfig …
├── src/renderer/            # 结果面板：最新划词结果 + 最近 20 条历史 + 设置
├── src/shared/types.ts      # ProbeRecord / AppConfig（主进程与渲染进程共用）
├── native/                  # Rust napi-rs 原生模块（workspace 成员）
│   ├── src/lib.rs           # UIA COM 探测核心（专用 COM MTA 工作线程 + 500ms 超时）
│   │                        #   导出 probeSelection（产品 4 字段）/ probeSelectionDetailed（审计）
│   └── examples/rect_probe.rs  # 无 GUI 诊断工具：枚举窗口内 TextPattern 元素，
│                               # 编程式 Select 后dump文本/坐标（用于排查精简模式）
├── testpage/                # 本地测试页（常规段落 / 中英混排 / input / textarea）+ sample.pdf
└── scripts/gen-pdf.mjs      # 生成 testpage/sample.pdf
```

## 运行

前置：Windows 10/11、Node 18+、pnpm、Rust (MSVC)。

```powershell
pnpm install
pnpm build:native        # cargo build --release + 复制 DLL 为 .node（需 MSVC BuildTools + Rust）
node scripts/gen-pdf.mjs # 可选：重新生成测试 PDF
pnpm start               # electron-vite dev，日志打到控制台与 uia-probe.log
```

启动后会同时打开一个**结果面板**窗口：显示最近一次划词结果（文本 / 目标应用 / rects / 耗时 / 错误码）、最近 20 条历史（点击回看），以及设置区。点关闭只是隐藏面板，进程和热键继续工作；按**面板热键**（默认 `Ctrl+Shift+1`）可重新唤出。

设置区可调：划词热键（默认 `Ctrl+Shift+2`，点击输入框后按组合键录入）、面板热键、超时毫秒数、窗口置顶；"保存配置"即生效并写入应用根目录的 `uia-config.json`（优先级：配置文件 > `UIA_HOTKEY`/`UIA_TIMEOUT_MS` 环境变量 > 默认值）。面板上的"立即探测"按钮等价于按划词热键。

启动后在 Chrome/Edge 打开任意网页（或 `testpage/index.html`），划选文字，按划词热键触发探测。每次触发输出一行 JSON：

```json
{"timestamp":"2026-09-13T12:00:00.000Z","level":"probe","seq":1,"trigger":"hotkey","success":true,
 "targetApp":"msedge.exe (pid 1234) — UIA-TEST","text":"selected text","rectCount":2,
 "rects":[{"x":100,"y":200,"width":300,"height":24}, ...],
 "elapsedMs":41,"nativeElapsedMs":12.3,
 "errorCode":null}
```

字段：`timestamp` 触发时间；`success` 布尔；`targetApp` 前台进程/标题；`text` 选中字符串；`rectCount` + `rects` 包围盒数量与各 Rect（物理像素，多行多个）；`elapsedMs` 端到端耗时；`errorCode` 失败原因（`Timeout` / `EmptySelection` / `TextPatternNotSupported` / `ElementNotFound` / `Busy` / …）。

失败路径也会输出日志（`success:false` + `errorCode`），空选区返回 `EmptySelection` 而非报错崩溃。探测进行中重复按键返回 `Busy`（不排队、不阻塞）。

## 超时与并发安全

- UIA 跨进程 COM 调用是同步阻塞的：所有探测在专用 **COM MTA 工作线程**执行（`napi::Task` → libuv 线程池 → channel 等待），Electron 主线程永不阻塞。
- 单次探测默认 **500ms 硬超时**（`UIA_TIMEOUT_MS` 可调）。超时后放弃该工作线程，下次调用自动换新线程，目标进程卡死不会拖挂本进程。
- 全程零剪贴板依赖：探测路径不包含任何剪贴板 API，UIA 跨进程读取选区，剪贴板不可能被触碰（由实现保证，无需运行时自证）。

## 已知兼容边界

- **Chromium 无障碍精简模式（重要）**：Chromium 默认不完整暴露 renderer 排版树。a11y 树完整激活前，Document 级选区可能取不到 bounding rectangles（文本仍可读）；树激活后（首次 UIA 树枚举 / 屏幕阅读器接入 / `--force-renderer-accessibility` 启动参数）文本与逐行坐标均完整返回。实测同一页面激活前后：`rectCount: 0` → `rectCount: 13`。
- **PDF 查看器（实测 Edge 137）**：内置 PDF 阅读器不通过 UIA TextPattern 暴露选区文本 —— 聚焦元素无 TextPattern，tree-search 回退可找到带 pattern 的元素但 `GetSelection` 返回的 range 无文本（`EmptySelection`）。标记为兼容边界。
- **多显示器**：坐标为物理像素，单显示器 200% DPI 环境下实测与屏幕位置精确对齐（页面内容左缘逻辑 160px → 返回 rect x=320）；第二显示器偏移未在本环境验证。
- Electron 宿主自身（本 app 无窗口）也可以作为"基于 Electron 的第三方软件"目标验证：焦点落在任何 Electron 窗口的网页区时同样走 Chromium 无障碍管线。

## 产品接口 vs 审计接口

原生模块导出两个函数，共用同一个 UIA worker / 500ms 超时机制：

**`probeSelection(timeoutMs?) → SelectionResult`** — 产品接口，划词翻译只需要这 4 个字段：

| 字段 | 说明 |
| --- | --- |
| `text` | 选中文本（空串 = 无有效选区） |
| `rects` | 逐行物理像素包围盒；`rects[0]` 即气泡锚点 |
| `errorCode` | `null` = 成功；否则 `EmptySelection` / `TextPatternNotSupported` / `Timeout` / … |
| `processName` | 前台应用进程名，用于按应用启停划词的规则过滤 |

**`probeSelectionDetailed(timeoutMs?) → ProbeResult`** — 审计接口（12 字段），仅供 PoC 验收日志使用：增加 `success`、`windowTitle`、`processId`、`focusedElement`（焦点元素类型与定位策略）、`nativeElapsedMs`（原生耗时分解）、`warnings`。字段含义见 `src/types/uia-probe.d.ts`。

原来的 `success: boolean` 在产品接口中由 `errorCode === null` 取代；`rectCount` 由 `rects.length` 取代。

## 实测数据（Windows 11 26100，Edge 137，200% DPI）

| 场景 | 结果 | 耗时 |
| --- | --- | --- |
| 网页多行划词（13 行全选） | 文本 482 字符 + 13 个逐行 rect | 12ms |
| 网页两行划词 | 文本 115 字符 + 2 个 rect | 3ms |
| `<input>` 双击选词 | 文本 + 1 个精确 rect | 4ms |
| `<textarea>` 双击选词 | 文本 + 1 个精确 rect | 3ms |
| 空选区点击 | `EmptySelection` 拦截，无崩溃 | 3ms |
| 无 TextPattern 前台窗口 | `TextPatternNotSupported` + 进程名 | <20ms |
| 3 个并发探测（Promise.allSettled） | 全部成功，无崩溃/死锁 | 15ms |
| 连续快速按键 ×3 | 全部成功返回 | 3ms/次 |
