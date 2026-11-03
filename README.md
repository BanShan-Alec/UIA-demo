# uia-probe — Windows UIA 划词提取 PoC（Chromium 专项）

验证通过 **Electron + Rust (napi-rs)** 调用 Windows UI Automation，无剪贴板污染地提取 Chromium 体系（Chrome / Edge / Electron 应用）选中文字及屏幕物理坐标。

## 结构

```
├── src/main/index.ts        # Electron 主进程：全局快捷键 + JSON 日志（走审计接口）
├── src/main/config.ts       # uia-config.json 读写（配置文件 > 环境变量 > 默认值）
├── src/preload/index.ts     # contextBridge：onProbeResult / onDotSelection / getState / getConfig …
├── src/renderer/            # 结果面板：圆点触发的选区 + 最新划词结果 + 历史 + 设置
├── src/shared/types.ts      # ProbeRecord / DotSelection / AppConfig（主进程与渲染进程共用）
├── native/                  # Rust napi-rs 原生模块（workspace 成员）
│   ├── src/lib.rs           # UIA COM 探测核心（专用 COM MTA 工作线程 + 500ms 超时）
│   │                        #   导出 probeSelection（产品 4 字段）/ probeSelectionDetailed（审计）
│   │                        #   / probeExpandUnits（ExpandToEnclosingUnit 审计）
│   │                        #   / probeRangeFromPoint（Q-6 单词兜底审计）
│   ├── src/dot.rs           # 原生 Win32 圆点窗口线程 + 全局鼠标钩子 + TSFN 回调
│   └── examples/rect_probe.rs  # 无 GUI 诊断工具：枚举窗口内 TextPattern 元素，
│                               # 编程式 Select 后dump文本/坐标（用于排查精简模式）
├── testpage/                # 本地测试页（常规段落 / 中英混排 / input / textarea / 跨段选区）+ sample.pdf
├── docs/dot-trigger-spec.md # 原生圆点触发入口的交互规格与验收标准
└── scripts/
    ├── gen-pdf.mjs          # 生成 testpage/sample.pdf
    └── expand-probe-e2e.mjs # ExpandToEnclosingUnit 自动化验证（CDP 选区 + 原生探针）
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

## 原生 Win32 圆点触发入口（原型）

`startDot` 在专用线程创建一个 8px 的原生 Win32 圆点（24px 热区）+ 全局鼠标钩子。鼠标在任意支持 UIA TextPattern 的应用里选中英文后，圆点出现在选区右下（多行选区取最后一个有效 rect，贴近鼠标结束位置）；点击圆点会读取实时选区与所在段落，打开结果面板并把数据透传过去。

详细交互规格与验收标准见 [docs/dot-trigger-spec.md](docs/dot-trigger-spec.md)。

```powershell
pnpm start
# 无前台窗口的自动化环境：用合成事件验证面板定位与渲染
$env:UIA_DOT_SELFTEST='1'; pnpm start
```

- 圆点窗口：`WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE`，常驻隐藏、复用，不抢焦点
- 显示条件：英文占比 ≥ 30%、选区非空、非密码框、UIA 成功、排除自身进程
- 面板已打开时再次点击新圆点：复用窗口、更新内容，并按新圆点位置移动
- 面板正常抢焦点（本轮原型取舍）

**验证边界**：`UIA_DOT_SELFTEST=1` 已验证面板卡片、面板定位与数据透传；原生窗口的创建/显示已确认。真实“鼠标划选 → 圆点 → 点击”链路需要在有前台窗口的交互式桌面里手动验证（CI/锁屏桌面下 `GetForegroundWindow()` 可能为 NULL）。

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

## 场景支持情况

### 已验证可用（文本 + 坐标完整返回）

- **Chrome / Edge 网页正文划词**（核心场景）：单行、多行（13 行全选 482 字符 + 13 个逐行 rect）、中英混排均正常；坐标为物理像素，单显示器 200% DPI 下与屏幕位置精确对齐（页面内容左缘逻辑 160px → rect x=320）。
- **Chromium 表单控件**：`<input>` / `<textarea>` 双击选词，文本 + 1 个精确 rect；**地址栏**选中的完整 URL 同样可提取（聚焦元素直接是 Edit 控件，走 `focused-element` 最短路径）。
- **Electron 应用网页区**：本 app 面板自身及任意基于 Electron 的第三方软件，焦点落在网页区时走同一条 Chromium 无障碍管线。
- **失败路径**：空选区返回 `EmptySelection`（不崩溃）；探测中重复触发返回 `Busy`（不排队）；目标窗口无 TextPattern 时返回 `TextPatternNotSupported` 并携带进程名。所有失败 <25ms，无超时挂起。

### 支持不佳 / 已知边界

- **VS Code（Electron 应用，辅助功能默认关闭）**：前台窗口枚举常直接返回 `TextPatternNotSupported`；个别 Edit 控件可见但选区多为光标态（`EmptySelection`）。在 VS Code 内启用完整的屏幕阅读器模式（`Shift+Alt+F1`）后理论上走 Electron 管线，但未实测，不承诺支持。
- **Chromium 内置 PDF 阅读器（实测 Edge 137）**：不通过 UIA TextPattern 暴露选区文本 —— 聚焦元素无 TextPattern，tree-search 回退可找到带 pattern 的元素但 `GetSelection` 返回的 range 无文本（`EmptySelection`）。
- **Chromium 无障碍精简模式（重要）**：Chromium 默认不完整暴露 renderer 排版树。a11y 树完整激活前，Document 级选区可能取不到 bounding rectangles（文本仍可读）；树激活后（首次 UIA 树枚举 / 屏幕阅读器接入 / `--force-renderer-accessibility` 启动参数）文本与逐行坐标均完整返回。实测同一页面激活前后：`rectCount: 0` → `rectCount: 13`。
- **两个展示层陷阱（文本提取本身不受影响）**：① 精简模式下可能"成功但 `rects.length === 0`"，产品侧用 `rects[0]` 做气泡锚点前需判空；② 多行折行边界可能产生 1px 宽的退化矩形，画高亮框前建议过滤宽度 ≤1px 的 rect。

### 未验证 / 超出 PoC 范围

- 传统 Win32 应用（记事本、Office、资源管理器等）：本 PoC 专项针对 Chromium 体系，未做适配与测试，不支持是预期行为。
- 多显示器第二屏偏移、管理员权限窗口（UIA 跨完整性级别访问）、HTA/嵌入式 WebView2 等场景均未验证。

## 产品接口 vs 审计接口

原生模块导出四个 probe 函数，共用同一个 UIA worker / 500ms 超时机制；另外导出 `startDot` / `stopDot` 管理原生圆点线程。

**`probeSelection(timeoutMs?) → SelectionResult`** — 产品接口，划词翻译只需要这 4 个字段：

| 字段 | 说明 |
| --- | --- |
| `text` | 选中文本（空串 = 无有效选区） |
| `rects` | 逐行屏幕包围盒；`rects[0]` 即气泡锚点。**不保证物理像素**：Chrome/Edge 156 在 175% DPI 下多数返回 screen CSS 像素，定位前需按 `devicePixelRatio` 归一化或检测 |
| `errorCode` | `null` = 成功；否则 `EmptySelection` / `TextPatternNotSupported` / `Timeout` / … |
| `processName` | 前台应用进程名，用于按应用启停划词的规则过滤 |

**`probeSelectionDetailed(timeoutMs?, windowTitle?) → ProbeResult`** — 审计接口（12 字段），仅供 PoC 验收日志使用：增加 `success`、`windowTitle`、`processId`、`focusedElement`（焦点元素类型与定位策略）、`nativeElapsedMs`（原生耗时分解）、`warnings`。`windowTitle` 仅用于自动化：按可见窗口标题子串定位，而不是读 `GetForegroundWindow()`。字段含义见 `src/types/uia-probe.d.ts`。

**`probeExpandUnits(timeoutMs?, windowTitle?) → ExpandProbeResult`** — 审计接口，仅用于 `ExpandToEnclosingUnit` 验证。它读取当前选区，分别以「选区原样」和「折叠到选区起点」两种模式，对 7 种 `TextUnit` 调用 `ExpandToEnclosingUnit`，返回每种单位的字符数、文本预览、矩形数量和 `equalsDocument`。`windowTitle` 仅用于自动化：按可见窗口标题子串定位，而不是读 `GetForegroundWindow()`（CI / 锁屏桌面没有前台窗口时必需）。

**`probeRangeFromPoint(x, y, timeoutMs?, windowTitle?) → RangeProbeResult`** — Q-6 审计接口：对绝对屏幕点调用 `TextPattern.RangeFromPoint`，再 `ExpandToEnclosingUnit(Word)`，返回 `word`、`text`、命中元素类型与 `via`（`element-from-point` / `target-pattern`）。`x/y` 的坐标空间与 UIA rect 相同（当前实测多为 screen CSS 像素），脚本会同时尝试原始值与 `× devicePixelRatio` 的物理值。

**`startDot(callback) / stopDot()`** — 原生圆点线程。`callback` 在每次点击圆点时收到 `DotSelectionEvent`：`text`、`rects`、`processName`、`paragraph`、`paragraphError`、`dotRect`（物理像素）。仅在 Electron 主进程调用。

原来的 `success: boolean` 在产品接口中由 `errorCode === null` 取代；`rectCount` 由 `rects.length` 取代。

## ExpandToEnclosingUnit 验证（Snapword Q-1）

验证 `ExpandToEnclosingUnit(Paragraph)` 的真实颗粒度，以及请求更大单位时是否会把整页文本送给模型。

```powershell
# 本地 testpage（默认 Chrome；加 --browser edge 跑 Edge 访客模式）
pnpm test:expand

# 真实页面：选区由页面内 JS 设置，探针结果写 JSON 并做断言
pnpm test:expand --url "https://en.wikipedia.org/wiki/UI_Automation" --selector "#mw-content-text p"
pnpm test:expand --url "https://github.com/BanShan-Alec/Snapword" --selector "article.markdown-body p"
pnpm test:expand --url "https://martinfowler.com/articles/microservices.html" --selector ".paperBody > p"
```

脚本会启动一个全新浏览器配置，开启 `--force-renderer-accessibility`，用 CDP 设置确定性选区，再调用原生探针。所有断言不通过时退出码为 1；原始结果写入 `expand-probe-report.json`（已在 `.gitignore`）。

**实测环境**：Windows 11 26100，Chrome 156.0.8078.49 / Edge 156.0.4314.50，175% DPI。以下数值为字符数（`textChars`）。

| TextUnit | testpage | Wikipedia 首段 | GitHub README 首段 | Martin Fowler《Microservices》首段 | 观察 |
| --- | --- | --- | --- | --- | --- |
| Character | 1 | 1 | 1 | 1 | 单字符 |
| Word | `"brown "` 6 / `"vexingly "` 9 | `"Microsoft "` 10 | `"Windows "` 8；CJK `"划"` 1 | `"Microservices"` 13 | 英文含尾随空格；后接标点时不含空格；CJK 按单字 |
| Line | 76 / 75 | 117 | 52 | 74 | 当前渲染行 |
| Format | 401 / 268（整段同格式）；嵌套 `<strong>` 内 8 | 29（`"Microsoft UI Automation (UIA)"`） | 52（整段同格式） | 655（**超过段落 608**） | 随格式连续性变化，可能跨段，不能当段落兜底 |
| Paragraph | 133 / 121 / 131 | 191 | 53 | 608 | 精确命中所在段落（含尾部换行） |
| Page | 1015 | 15435 | 3142 | 45527 | **等于整个 Document** |
| Document | 1015 | 15435 | 3142 | 45527 | 整个文档 |

**实际案例（Martin Fowler《Microservices》）**：在首段选中单词 `Microservices`（13 字符），Paragraph 返回首段全文 **608 字符**；同页 Document 为 **45,527 字符**（约 75 倍），Page 与 Document 完全相等。页面动态内容会让 Document 文本在两次探测间从 45,790 降到 45,527，但 Paragraph 稳定在 608。如果误用 Page 或整页兜底，送给模型的上下文会从约 600 字符膨胀到约 4.5 万字符（约一万 token）。

**场景结论**

| 场景 | Paragraph 结果 |
| --- | --- |
| 单词 / 半句 / 整段选区（起点在段落 A） | 段落 A 全文（133），不退化到 Page/Document |
| 跨段选区（A 中部 → B 中部） | **只返回起点所在段落 A（133）**，196 字符选区剩余部分被丢弃 |
| 跨段选区（B 中部 → C 中部） | **只返回 B（121）**，156 字符选区剩余部分被丢弃 |
| 选中嵌套 `<strong>` 内的词 | 仍扩到整个 `<p>`（131），可跨内联格式 |
| `selection` vs `collapsed-start` | 上述结果完全一致 |

### 对 Snapword 的直接结论

1. **Q-1 担心的「Paragraph 退化成 Page/Document」在 Chrome/Edge 156 上未复现**。Paragraph 返回的是精确段落，可以作为「取所在段落」的可靠单位；但仍应在目标浏览器版本上用本探针复测。
2. **真正的整页风险在 `Page`**：它在网页内容上等于 `Document`。不要把 `Page` 当 Paragraph 请求失败后的兜底；兜底应是本地切句 / 输入框，而不是更大的 UIA 单位。
3. **跨段选区会被 Paragraph 截断到起点段**。不能假设 `ExpandToEnclosingUnit` 只做规范化、不会丢内容。用户选了多段时，应使用原选区本身，或按段分别处理，而不是对整段选区调用 Paragraph。
4. **文本需要清洗**：Paragraph 末尾带 `\n`（133 vs DOM 132），Word 带尾随空格；CJK 的 Word 粒度是单字。发给模型前统一 `trim()`；单词模式不要依赖 Word 单位做英文单词边界。
5. **自动化前置条件**：Chromium 默认是精简无障碍树。没有 `--force-renderer-accessibility` + CDP 焦点仿真（`Emulation.setFocusEmulationEnabled`）+ 页面内真实点击时，UIA 只能看到地址栏 Edit，看不到网页 Document。D-1 探针脚本必须把这三步写进去。

### 覆盖范围与未测项

- 已测：Chrome 156 / Edge 156；testpage（英文段落、CJK、嵌套 inline、跨段选区）、Wikipedia 首段、GitHub README 首段、Martin Fowler《Microservices》首段；同源 iframe（D-1）。
- 未测：跨域 iframe、canvas/PDF、管理员权限窗口、多显示器第二屏、Chrome <138、企业 `UiAutomationProviderEnabled` 策略回退。

## D-1 验收结果（2026-11-03，Chrome/Edge 156）

D-1 的验收标准（来自 Snapword PRD）：

- 3 类页面各选 10 次（单词 5 次、句子 5 次），共 30 次
- 取词成功率 ≥ 90%
- 单次取词耗时 ≤ 50ms
- 不产生任何剪贴板写入
- 附带验证 `RangeFromPoint`（Q-6）与 iframe 场景

3 类页面：**普通文章页（英文维基）**、**GitHub README（microsoft/vscode）**、**含 iframe 的本地页面**。

### 结果

| 浏览器 | 成功 | P50 | P95 | Max | Q-6 | 剪贴板写入 |
|---|---:|---:|---:|---:|---:|---:|
| Chrome 156.0.8078.49 | **30/30 (100%)** | 1.7ms | 16.2ms | 30.1ms | 28/30 (93.3%) | 0 |
| Edge 156.0.4314.50 | **30/30 (100%)** | 1.0ms | 24.2ms | 26.3ms | 28/30 (93.3%) | 0 |

分页面（Chrome / Edge）：

| 页面 | 成功 | P95（Chrome / Edge） | Q-6（Chrome / Edge） |
|---|---:|---:|---:|
| 普通文章页 | 10/10 / 10/10 | 30.1ms / 10.0ms | 9/10 / 8/10 |
| GitHub README | 10/10 / 10/10 | 10.4ms / 26.3ms | 9/10 / 10/10 |
| 含 iframe 页面 | 10/10 / 10/10 | 16.2ms / 24.2ms | 10/10 / 10/10 |

**结论：D-1 在 Chrome/Edge 156 上通过。** 30 次取词全部命中，P95 低于 50ms，未观察到剪贴板写入；iframe 内选区可以读取，`RangeFromPoint` 可用率 93.3%（重复运行 28–29/30）。

### 复现

```powershell
pnpm build:native
pnpm test:d1 --browser chrome
pnpm test:d1 --browser edge
```

结果写入 `d1-probe-report-*.json`（已忽略），包含每次选区的文本、rect、耗时、`rectSpace`、Q-6 尝试与错误码。

### 两个重要发现

1. **iframe 需要按 Document 找 TextPattern，而不是取第一个 Document。** 父页面的 Document 在树序中先出现，`GetSelection` 为空；探针现在先扫 Document，再扫全部 TextPattern，优先选择有非空 selection 的那个。
2. **不要假定 UIA rect 一定是物理像素。** 在 175% DPI 下，Chrome/Edge 156 返回的 rect 多数与 screen CSS 像素吻合（Chrome 26/30、Edge 28/30），少数样本与物理像素吻合。Q-6 脚本同时尝试 `rect * devicePixelRatio` 与原始 rect，才达到 93.3%–96.7%。产品做圆点/卡片定位时必须做坐标空间归一化或检测，不能直接当物理像素用。
3. **Q-6 失败点集中在文章页 / GitHub README 的单词边界（每次运行 1–2 次）。** 属于选区起点/单词边界的点位问题，不是 `RangeFromPoint` 整体不可用；当前结果支持把它作为兜底能力，但需要失败重试/二次取点。

### 未覆盖

- Q-7：Chrome <138、企业 `UiAutomationProviderEnabled` 策略回退未测。
- Q-2：全屏应用下卡片置顶未测。
- 跨域 iframe、canvas/PDF、管理员权限窗口、多显示器第二屏未测。



## 原有划词实测数据（Windows 11 26100，Edge 137，200% DPI）

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
