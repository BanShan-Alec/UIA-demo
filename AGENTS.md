# AGENTS.md

本文件为自动化 Agent（及开发者）提供 `uia-probe` (UIA-demo) 项目的核心背景、技术架构、目录索引、常用指令与开发规范，避免在新会话或开发中重复进行全项目递归扫描。

---

## 1. 项目概览 (Project Overview)

- **项目名称**：`uia-probe` (UIA-demo)
- **定位**：Windows 平台针对 **Chromium 架构应用（Google Chrome、Microsoft Edge、Electron 应用等）** 的**无剪贴板污染划词与物理坐标提取**验证原型项目（PoC）。
- **核心目的与价值**：
  - **零剪贴板破坏 (Zero-Clipboard Pollution)**：彻底摆脱传统取词/翻译软件模拟 `Ctrl+C` 按键读取剪贴板的方案，避免破坏用户的系统剪贴板历史或被安全软件阻断。
  - **物理像素逐行矩形 (Bounding Rectangles)**：不仅提取选中文本，同时获取每一行在屏幕上的绝对物理像素包围盒（DPI 感知），为悬浮划词气泡、取词工具栏提供像素级精确定位。
  - **COM MTA 隔离与硬超时 (MTA Worker & Hard Timeout)**：UIA 跨进程 COM 调用是同步阻塞调用。项目在专用 COM MTA（多线程单元）工作线程中执行探测，配置默认 500ms 硬超时，超时自动隔离孤儿线程并启动新线程，确保主进程不被目标应用卡死。
  - **分层接口 (Two-tier API)**：提供精简产品级接口（`probeSelection`，仅 4 个字段）与审计诊断接口（`probeSelectionDetailed`，包含 12 项指标如 PID、路径、耗时分段等）。
  - **ExpandToEnclosingUnit 段落粒度验证 (Q-1)**：`probeExpandUnits` 对 7 种 `TextUnit` 做审计；`scripts/expand-probe-e2e.mjs` 在真实 Chromium 页面上断言 Paragraph 不退化成 Page/Document，并记录跨段选区的截断行为。
  - **原生 Win32 圆点触发入口（原型）**：`startDot` 在专用线程创建 8px 原生圆点 + 全局鼠标钩子；鼠标选中英文后圆点出现在选区右下，点击读取实时选区与段落，打开结果面板并透传数据。交互规格见 `docs/dot-trigger-spec.md`。

---

## 2. 技术栈架构 (Tech Stack & Architecture)

| 层级 | 技术选型 | 说明 |
| :--- | :--- | :--- |
| **原生内核 (Native)** | Rust 2021, `windows` 0.58, `napi-rs` 2.16 | 调用 Windows 原生 UIA COM 接口，编译为 `.node` 原生 Node 插件 |
| **应用容器 (Host)** | Electron 37, TypeScript 5.5 | 管理生命周期、全局热键、配置持久化与日志落盘 |
| **构建工具** | Vite 6, `electron-vite` 3 | 前端构建与主进程/预加载打包 |
| **前端展示 (UI)** | 原生 HTML/CSS + TypeScript | 实时展示探测结果、包围盒详情、热键修改与配置表单 |

---

## 3. 核心目录与文件索引 (Directory Structure)

```
H:\UIA-demo\
├── native/                  # Rust 原生模块（UIA 核心）
│   ├── src/lib.rs           # COM MTA 初始化、UIA 探测算法、矩形转换与 napi 导出
│   ├── src/dot.rs           # 原生 Win32 圆点窗口线程、鼠标钩子、ThreadsafeFunction 回调
│   ├── examples/rect_probe.rs # 命令行独立诊断工具（无 GUI，调试 TextPattern 元素与坐标）
│   ├── Cargo.toml           # windows 0.58, napi, napi-derive 等依赖
│   └── package.json         # napi-rs 构建配置
├── docs/
│   └── dot-trigger-spec.md  # 原生圆点触发入口的交互规格与验收标准
├── src/
│   ├── main/                # Electron 主进程
│   │   ├── index.ts         # 注册全局快捷键、IPC 通信、日志落盘 (uia-probe.log)
│   │   └── config.ts        # 配置管理 (uia-config.json / 环境变量 / 默认值三级降级)
│   ├── preload/             # 预加载脚本
│   │   └── index.ts         # contextBridge 暴露类型安全 API (window.api)
│   ├── renderer/            # 调试与结果面板前端
│   │   ├── index.html       # 状态展示、设置表单、历史记录
│   │   └── src/main.ts      # 结果渲染、包围盒可视化详情、热键录入与持久化
│   ├── shared/              # 跨进程通用类型
│   │   └── types.ts         # ProbeRecord, SelectionResult, AppConfig 等 TypeScript 契约
│   └── types/
│       └── uia-probe.d.ts   # 原生 napi 模块的 TypeScript 类型定义
├── testpage/                # 本地测试页面
│   ├── index.html           # 包含丰富 HTML 段落、中英混排、输入框的测试页
│   ├── iframe.html          # iframe 父页面（D-1 第 3 类页面）
│   ├── iframe-child.html    # iframe 子文档（英文段落，供选区探针使用）
│   └── sample.pdf           # 用于测试 Edge/Chrome 内置 PDF 插件取词边界的样本文件
├── scripts/                 # 构建与辅助脚本
│   ├── build-native.mjs     # 编译 Rust 原生模块并移动 .node 文件
│   ├── gen-pdf.mjs          # 生成测试用 PDF 文件
│   ├── expand-probe-e2e.mjs # ExpandToEnclosingUnit 自动化验证（CDP 选区 + 原生探针）
│   └── d1-feasibility.mjs   # D-1 30 次统计 + Q-6 RangeFromPoint + iframe 验证
├── uia-config.json          # 本地持久化配置（热键、超时时间等，存在默认降级）
├── uia-probe.log            # 运行日志（每次探测的耗时与结果 JSON）
└── README.md                # 完整的技术调研报告、场景兼容矩阵与实测耗时数据
```

---

## 4. 常用命令与开发工作流 (Commands & Workflow)

在根目录下执行：

- **安装前端依赖**：
  ```bash
  npm install
  ```
- **构建原生 Rust 模块**：
  ```bash
  npm run build:native        # Release 构建（生产/测试推荐）
  npm run build:native:debug  # Debug 构建（开发快速迭代）
  ```
- **启动 Electron 调试环境**：
  ```bash
  npm run dev                 # 启动 Vite + Electron 开发窗口并开启热重载
  ```
- **运行 Rust 原生诊断命令行（免启动 Electron）**：
  ```bash
  cargo run --manifest-path native/Cargo.toml --example rect_probe
  ```
- **ExpandToEnclosingUnit 自动化验证**（默认 Chrome；支持 `--browser edge`、`--url`、`--selector`）：
  ```bash
  pnpm test:expand
  # 等价于 node scripts/expand-probe-e2e.mjs
  ```
- **D-1 自动化统计**（3 类页面 × 5 单词 + 5 句子 + Q-6；支持 `--browser chrome|edge`）：
  ```bash
  pnpm test:d1
  pnpm test:d1 --browser edge
  ```
- **原生圆点原型**：
  ```bash
  pnpm start
  # 无前台窗口时用合成事件验证面板定位/渲染
  $env:UIA_DOT_SELFTEST='1'; pnpm start
  ```
- **代码类型检查与打包**：
  ```bash
  npm run typecheck
  npm run build
  ```

---

## 5. 关键设计原则与已知兼容边界 (Constraints & Gotchas)

1. **坚守无剪贴板污染原则**：
   - 严禁引入任何使用 `keybd_event`、`SendInput` 发送 `Ctrl+C` 的逻辑或降级方案。所有文本与坐标抓取必须通过 UIA 原生接口实现。
2. **COM 与线程安全**：
   - Windows UIA COM 是同步阻塞调用。绝不能在 Electron 主线程/UI 线程直接调用未做超时的 COM 接口。Native 模块必须维持 MTA 独立工作线程机制。
3. **Chromium 无障碍树激活边界 (A11y Tree Activation)**：
   - Chromium 默认处于惰性/精简无障碍模式。首次可能出现文本能够通过 TextPattern 读取但物理矩形数组为空（`rectCount: 0`）的情况；一旦 Chromium 触发无障碍树完全激活（或开启屏幕阅读器支持），逐行物理坐标即可稳定提取。
4. **坐标空间与 DPI 缩放**：
   - UIA `BoundingRectangles` **不保证物理像素**。Chrome/Edge 156 在 175% DPI 下，D-1 的 30 个样本中多数与 screen CSS 像素吻合（Chrome 26/30、Edge 28/30），少数与物理像素吻合。定位圆点/卡片前必须做坐标空间检测或归一化；不要直接当物理像素用。
5. **PDF 阅读器支持限制**：
   - Chrome / Edge 内置的 PDF Viewer 使用了独立的虚拟渲染管线，通常不暴露标准的 TextPattern 选区，属于已知的兼容边界。
6. **Chromium 无障碍树激活（自动化前提）**：
   - 默认精简模式下，UIA 顶层窗口树只有地址栏等浏览器 UI，看不到网页 Document。自动化探针必须同时具备：`--force-renderer-accessibility` 启动参数、CDP `Emulation.setFocusEmulationEnabled`、页面内真实点击（`Input.dispatchMouseEvent`）；缺一不可。`scripts/expand-probe-e2e.mjs` 已封装这三步。
7. **ExpandToEnclosingUnit 的真实语义（Chrome/Edge 156 实测）**：
   - `Paragraph` 精确命中选区起点所在段落，单段选区不会退化到 Page/Document；但**跨段选区会被截断到起点段**，不会保留整段选区。
   - `Page` 在网页内容上等于整个 `Document`，不能作为 Paragraph 的兜底单位。
   - Paragraph 文本末尾带 `\n`，Word 带尾随空格；CJK 的 Word 粒度是单字。
8. **原生圆点窗口（原型）**：
   - `native/src/dot.rs` 在专用线程创建 `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE` 的 24px 窗口，内部画 8px 圆点；窗口常驻隐藏、复用，不重复创建/销毁。
   - 全局鼠标钩子（`WH_MOUSE_LL`）在 mouse-up 时同步调用 UIA probe（150ms 超时）；点击圆点在 `WM_LBUTTONDOWN` 里读实时选区 + 段落，再通过 `ThreadsafeFunction` 回调 Electron main。
   - 面板正常抢焦点（本轮原型取舍）；真实鼠标选区链路需在交互式桌面手动验证，CI/锁屏桌面下 `GetForegroundWindow()` 可能为 NULL。
   - mouse-up 全局钩子里同步 probe（150ms 超时）是原型简化；产品应把 probe 移到异步事件源，避免慢应用拖慢鼠标输入。
   - 选区清除检测在原型里用 600ms 低频复查兜底（鼠标路径立即隐藏）；产品应改用 `UIA_Text_TextSelectionChangedEventId`。
   - 圆点不可拖拽；只有 8px 视觉 + 24px 热区；hover 手型光标；无 tooltip、无放大。
9. **D-1 已通过（2026-11-03，Chrome/Edge 156）**：
   - 30/30 取词成功；P95 16.2ms（Chrome）/ 24.2ms（Edge），max 30.1/26.3ms；零剪贴板写入。
   - Q-6 `RangeFromPoint` 28/30（93.3%；重复运行 28–29/30）；iframe 内选区 10/10。
   - Q-6 失败点集中在文章页 / GitHub README 的单词边界（每次运行 1–2 次），属于选区起点/单词边界的点位问题，不是整体不可用。
   - 未覆盖：Q-7 Chrome <138/企业策略、Q-2 全屏置顶、跨域 iframe。

---

## 6. Agent 协作规范 (Agent Guidelines)

- **类型真理来源**：
  - Rust 导出的原生方法定义以 `src/types/uia-probe.d.ts` 为准。
  - Electron 内部与 IPC 通信契约以 `src/shared/types.ts` 为准。修改任一方时必须同步保证双向类型一致。
- **排查与调优定位**：
  - 若需要修改探测流程、重试策略或超时熔断机制，核心文件位于 `native/src/lib.rs`。
  - 若需要调整快捷键响应、IPC 派发或配置读写，核心文件位于 `src/main/index.ts` 与 `src/main/config.ts`。

---

## 7. Native 模块迁移指南 (Native Migration Guide)

本项目的全部验证价值集中在 `native/`（Rust UIA 探测内核）。将其迁移到新项目时，按以下清单执行，**不要只复制 `lib.rs`**。

### 7.1 必须迁移的文件清单（缺一不可）

| 源文件 | 作用 | 迁移说明 |
| :--- | :--- | :--- |
| `native/src/lib.rs` | 核心算法：COM MTA 初始化、MTA 工作线程、硬超时熔断、TextPattern 双重探测策略、矩形转换、napi 导出 | 整体复制，无对本仓库的任何路径依赖 |
| `native/build.rs` | napi 构建钩子（`napi_build::setup()`） | 2 行样板，必带 |
| `native/Cargo.toml` | 依赖锁定：`napi` 2.16 (features `napi8`, `default-features = false`)、`napi-derive`、`windows` 0.58（7 个 `Win32_*` features）| 直接复制；`crate-type` 必须保持 `["cdylib"]` |
| `native/index.js` | 按平台加载 `.node` 二进制的入口 | 直接复制；若重命名 crate，需同步改 `require` 的文件名 |
| `native/package.json` | napi-rs 包配置（`main`/`types` 指向） | 直接复制，改名 package name |
| `scripts/build-native.mjs` | 绕开 `@napi-rs/cli` 的构建脚本：`cargo build --release` 后把 `uia_probe.dll` 复制为 `uia_probe.win32-x64-msvc.node` | 直接复制。注意末尾产物文件名 = `{crate名下划线化}.{平台三元组}.node`，改名时两处必须同步 |
| `examples/rect_probe.rs`（可选） | 免 GUI 的命令行诊断工具 | 强烈建议带走，新环境首次联调时用于快速隔离 UIA 问题 |
| `scripts/d1-feasibility.mjs`（可选） | D-1 30 次统计 + Q-6 + iframe 的自动化验收脚本 | 建议带走；需同步 `testpage/iframe*.html` |
| `src/types/uia-probe.d.ts` | napi 模块的 TS 类型契约 | 复制到新项目对应位置；`declare module 'uia-probe'` 的模块名需与新包名一致 |

### 7.2 迁移时的关键设计红线（验证过的行为，勿改动）

1. **MTA 工作线程 + 硬超时结构**：`lib.rs` 的架构是「JS 侧 `napi::Task`（libuv 线程池）→ mpsc channel → 专用 COM MTA 线程执行同步 UIA 调用」。超时后**放弃孤儿线程、下次调用换新线程**。新项目若改成直接在 Task 里做 COM 调用（不经过 MTA worker channel），目标应用卡死时会拖死宿主进程——这是本方案的核心卖点，不可简化。
2. **COM 初始化单元**：工作线程内必须是 `CoInitializeEx(COINIT_MULTITHREADED)`（MTA），而非 STA。
3. **零剪贴板原则**：`lib.rs` 全文无任何剪贴板 API，迁移时同样禁止引入 `SendInput`/`keybd_event` 模拟 `Ctrl+C` 的降级路径。
4. **双重定位策略**：先 `GetFocusedElement` 直取 TextPattern（最快路径），失败则回退到前台窗口子树检索（`tree-search`）。标题/自动化路径会先找 Document，再扫描全部 TextPattern，优先选择**当前有非空 selection** 的那个（iframe 的 Document 不是树序第一个）。`FocusInfo.via` 字段记录命中策略。
5. **防御性上限**：`MAX_TEXT_CHARS = 10_000`、`MAX_RECTS = 4_096`，防止异常目标返回超大 SAFEARRAY 拖垮内存。
6. **`default-features = false`**：`napi` crate 必须关闭默认 features 只留 `napi8`，避免引入不必要的 tokio/异步运行时。
7. **标题路径短缓存**：`run_probe_for_title` 对 `(window_title, TextPattern)` 做 5s 线程内缓存；命中时探测 0.5–4ms，未命中 15–60ms。缓存失效（`EmptySelection`/`ElementNotFound`/`ComError`）时清缓存并重定位一次。产品前台路径不走这个缓存。

### 7.3 接入新宿主项目的步骤

1. **包结构**：新项目若用 pnpm workspace，把 `native/` 注册进 `pnpm-workspace.yaml`，宿主 `package.json` 中以 `"uia-probe": "workspace:*"` 引用；非 workspace 项目可直接把 `native/` 当独立 npm 包（file 协议）引用。
2. **构建接线**：把 `scripts/build-native.mjs` 挂到新项目 `package.json` 的 `build:native` script，并在 `build`/`dev` 前置执行（或用 `prebuild` 钩子）。
3. **进程接线**（以 Electron 为例，其他框架同构）：
   - 只在**主进程** `require('uia-probe')`——native 模块不能进 renderer；
   - 参照本项目 `src/main/index.ts`：注册全局热键/触发器 → 调 `probeSelectionDetailed(timeoutMs, windowTitle?)` → 结果经 IPC 推给 UI；
   - renderer 一律通过 `contextBridge`（preload）访问，参照 `src/preload/index.ts`。
4. **消费哪个接口**：产品代码只消费 `probeSelection()`（4 字段）；`probeSelectionDetailed()` / `probeExpandUnits()` / `probeRangeFromPoint()` 仅用于日志/审计。不要把 `ProbeResult` 的 12 个字段直接透传进产品 UI。
5. **重命名检查表**：若 crate 从 `uia-probe` 改名，需同步 4 处——`Cargo.toml [package] name`、`scripts/build-native.mjs` 的 dll→node 复制文件名、`native/index.js` 的 require 路径、TS 声明文件 `declare module '<新包名>'`。

### 7.4 环境与平台约束

- **仅限 Windows**：依赖 `Win32_UI_Accessibility`，无法跨平台编译；CI/打包必须用 `windows-latest`（VS Build Tools + Windows SDK + Rust MSVC 工具链均预装）。
- **仅支持 x64 MSVC**：产物名固定为 `win32-x64-msvc`。ARM64 需加 target `aarch64-pc-windows-msvc` 并产出 `uia_probe.win32-arm64-msvc.node`。
- **N-API ABI 稳定**：使用 napi（而非 neon），编译一次即可跨 Node/Electron 版本复用，**无需针对 Electron 重编译**。
- **DPI / 坐标语义**：UIA 返回的 rect **不保证物理像素**。Chrome/Edge 156 在 175% DPI 下实测多数为 screen CSS 像素；新宿主若用物理像素做窗口定位，必须先做坐标空间检测或按 `devicePixelRatio` 归一化。`RangeFromPoint` 的入参也需要同时尝试两种空间。
- **目标端前提**：被探测的 Chromium 应用需激活无障碍树（首次探测出现 `text` 有值但 `rectCount: 0` 即此边界）；内置 PDF Viewer 不支持。
