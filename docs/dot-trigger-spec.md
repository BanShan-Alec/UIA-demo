# 原生 Win32 小圆点触发入口 · 交互规格

> 状态：已对齐，按此实现原型
> 适用范围：`uia-probe`（UIA-demo）原型；目标产品为 Snapword
> 更新日期：2026-11-02

## 1. 目标

在选区旁显示一个原生 Win32 小圆点，作为划词翻译的鼠标触发入口。

点击圆点后：

1. 读取当前选区与所在段落；
2. 打开已有的结果面板；
3. 把选词结果透传给面板。

圆点本身不翻译，也不承载译文展示。

## 2. 非目标

- 翻译面板内部的交互（翻译按钮、流式展示、错误态等）不属于本 demo
- 不接真实翻译 API，不做 mock 译文
- 不做键盘选区触发（只覆盖鼠标 mouse-up）
- 不做圆点拖拽
- 不做 hover 放大、tooltip
- 不做多显示器混合 DPI 的精细校准，只保证不跑出工作区

## 3. 核心交互

### 3.1 出现

1. 用户在任意支持 UIA TextPattern 的应用里用鼠标选中文本。
2. 全局鼠标钩子收到 `WM_LBUTTONUP`。
3. 调用 `probeSelection` 读取选区。
4. 满足全部显示条件时，在选区旁显示圆点；否则隐藏圆点。

显示条件：

| 条件 | 要求 |
|---|---|
| 应用 | 任意有 UIA TextPattern 的应用（不限 Chrome/Edge） |
| 选区 | 非空、trim 后非空 |
| 语言 | 英文占比 ≥ 30%（沿用 Snapword FR-11 口径）；单词和句子都显示 |
| 安全 | 不是密码框 |
| 结果 | `probeSelection` 成功，无 errorCode |
| 自身 | 不显示在本应用自己的窗口上（排除自身进程） |

不显示：非英文、空选区、密码框、UIA 错误、自身进程。

### 3.2 外观

| 项 | 规格 |
|---|---|
| 视觉尺寸 | 8px 圆点 |
| 点击热区 | 24×24px 透明窗口 |
| 颜色 | 黑色圆芯 + 1px 白色描边 |
| 鼠标 | hover 时手型光标 |
| 动效 | 无放大、无 tooltip、无 loading 动画 |
| 拖拽 | 不支持 |

### 3.3 位置

圆点窗口为 24×24，视觉圆点在窗口内居中。

- 默认：窗口左上角 = 选区右下角 + 8px
- 多行选区：取最后一个有效 rect 的右下角（贴近鼠标结束位置；过滤宽度 ≤1px 的折行退化 rect）
- 右侧越界：翻到选区左侧
- 下方越界：翻到选区上方
- 最后按选区所在显示器的 work area 做 clamp

### 3.4 点击

在圆点窗口的 `WM_LBUTTONDOWN` 里：

1. 立即隐藏圆点。
2. 用点击瞬间捕获的目标窗口句柄同步读取实时选区（不缓存）。
3. 读取所在段落（`ExpandToEnclosingUnit(Paragraph)`）。
4. 成功：通过 `ThreadsafeFunction` 把结果发给 Electron main，打开/移动/聚焦结果面板。
5. 失败：不打开面板。

技术约束：

- 圆点窗口必须 `WS_EX_NOACTIVATE`，避免点击本身抢焦点。
- 点击时优先使用 mouse-up 时记录的目标窗口句柄读取选区，避免焦点变化导致读错窗口。
- 不缓存 selection；选区消失，圆点也消失。

### 3.5 消失

以下任一条件立即隐藏圆点：

- 点击圆点
- 选区被清除
- 点击圆点之外的区域
- 页面/内容滚动
- 前台窗口切换
- 进入全屏应用
- 鼠标抬起后探测失败或显示条件不满足

选区清除的鼠标路径（点击、重新划选）由鼠标钩子立即处理；Esc 清除选区等无鼠标事件的情况，由原型每 600ms 的低频复查兜底（产品应改用 `UIA_Text_TextSelectionChangedEventId`）。

不设置“鼠标移出圆点 800ms 后隐藏”。

## 4. 原生窗口

- 独立 Win32 窗口线程，与 UIA COM worker 分离
- 窗口常驻隐藏，显示/隐藏使用 `ShowWindow` / `SetWindowPos` 复用，不重复创建/销毁
- 窗口样式：
  - `WS_POPUP`
  - `WS_EX_LAYERED`
  - `WS_EX_TOOLWINDOW`
  - `WS_EX_TOPMOST`
  - `WS_EX_NOACTIVATE`
- 不进入 Alt+Tab / 任务栏
- `WM_MOUSEACTIVATE` 返回 `MA_NOACTIVATE`
- `WM_SETCURSOR` / `WM_MOUSEMOVE` 使用手型光标
- 使用 `UpdateLayeredWindow` 绘制 8px 圆点 + 1px 白描边

## 5. 结果面板

- 复用 `uia-probe` 现有结果面板窗口
- 面板正常抢焦点（已确认 Q15 B）
- 新增「圆点触发的选区」卡片，展示：
  - `text`
  - `rects`
  - `processName`
  - `paragraph`
  - `paragraphError`（段落读取失败时显示“语境不可用”）
- 不做翻译 UI，不做 mock 译文

### 5.1 面板跟随圆点

面板已经打开时再次点击圆点：

- 复用同一个面板窗口，不新开窗口、不闪烁
- 更新内容
- 把面板移动到新圆点的位置

定位规则：

- 面板左边 = 圆点窗口右边 + 8px
- 面板垂直中心 = 圆点窗口中心
- 右侧放不下时翻到圆点左侧
- 上下按圆点所在显示器的 work area 做 clamp
- 第一次打开也使用同一规则
- 立即 `setPosition`，不做滑动动画
- 面板保持当前尺寸（默认 440×640），超出工作区时只 clamp，不缩放

## 6. 数据契约

Electron main 收到的圆点事件：

```ts
interface DotSelectionEvent {
  text: string
  rects: SelectionRect[]
  processName: string
  paragraph: string | null
  paragraphError: string | null
  dotRect: SelectionRect // 物理像素，圆点窗口的屏幕矩形
}
```

## 7. 实现结构（本 demo）

```text
native/src/dot.rs     # 原生 Win32 圆点窗口线程、鼠标钩子、TSFN 回调
native/src/lib.rs     # 复用 / 新增按 HWND 读取选区与段落的 probe
src/main/index.ts     # 接收圆点事件，打开/移动/聚焦面板
src/preload/index.ts  # 暴露 onDotSelection / getLatestDotSelection
src/renderer/         # 「圆点触发的选区」卡片
```

技术选择：

- 圆点窗口放在现有 Rust napi addon 的专用线程里（原型最快）
- 点击结果通过 `ThreadsafeFunction` 回调 Electron main
- snapword 产品化时再迁到独立宿主进程

调试入口：

```powershell
$env:UIA_DOT_SELFTEST='1'; pnpm start
```

该模式用合成事件验证「圆点事件 → 面板卡片 → 面板定位/移动」，不依赖真实鼠标选中。

## 8. 验收标准

- [ ] 鼠标选中英文后，圆点出现在选区右下方，不遮挡选中文字
- [ ] 圆点窗口不抢焦点；浏览器选区在点击前仍然存在
- [ ] 点击圆点后圆点立即隐藏，面板打开并显示选区数据
- [ ] 面板已打开时再次点击新圆点，面板移动到新位置并更新内容
- [ ] 非英文、空选区、密码框、UIA 错误、自身进程不显示圆点
- [ ] 页面滚动、切换窗口、全屏应用时圆点隐藏
- [ ] 不重复创建/销毁圆点窗口
- [ ] 175% DPI 下圆点位置与选区对齐，不跑出工作区
- [ ] 不产生剪贴板写入

> 2026-11-02 原型验证：`UIA_DOT_SELFTEST=1` 已验证「圆点事件 → 面板卡片 → 面板定位/移动」；原生窗口创建与显示已确认。真实「鼠标划选 → 圆点 → 点击」链路需要在有前台窗口的交互式桌面手动验证。

## 9. 已知限制

- 点击圆点可能改变前台焦点；原型通过点击瞬间读取 + 目标 HWND 规避，仍需实测
- mouse-up 全局钩子里同步 probe（150ms 超时）是原型简化；产品应把 probe 移到异步事件源，避免慢应用拖慢鼠标输入
- 选区清除检测：原型用 600ms 低频复查兜底；产品应改用 UIA 选择变化事件
- iframe、canvas、PDF、管理员权限窗口未覆盖
- 键盘选区未覆盖
- 多显示器混合 DPI 只做 work area clamp
- 面板抢焦点与 Snapword 最终“侧边栏不夺焦”的体验不同，这是本 demo 的临时取舍
