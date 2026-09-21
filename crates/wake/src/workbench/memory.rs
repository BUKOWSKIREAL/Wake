//! Memory 页:agent 自己写下的记忆文档(Claude Code 的 auto-memory、Codex 的
//! memories)的只读浏览。侧栏底部入口,整页目的地——与 Insights 同形制:替换中栏与
//! 阅读区、与导航行互斥(`Workbench::page`)。左列按项目分组的文件列表,右侧阅读面
//! 渲染 Markdown。数据来自 memories 表(scanner 每轮扫描收尾刷新),正文阅读时先读
//! 磁盘、读不到用库里那份。只看不改:动作只有 Reveal 与 Copy path——写别家的记忆
//! 是别家的事(用户 2026-09-17 定:整页目的地、只读;MCP 面另有 wake_list_memories)

use super::*;
use wake_core::adapters::memory_body;
use wake_core::models::{MemoryCounts, MemoryDoc, MemoryFilter, MemoryGroup, MemoryScope};

/// 左列的一行:分组头(带该组第一份文档的下标,标签从它算)或一份文档
#[derive(Clone, Copy)]
enum MemoryRow {
    Group(usize),
    Doc(usize),
}

/// 选中文档在后台读好的部分:正文(SharedString——TextView 每帧要一份,Arc 一拨就够),
/// 以及解析过的来源路径(SQLite 型的虚拟路径落到库文件本体——解析要 stat,不能在
/// render 里每帧做)
struct LoadedMemory {
    body: SharedString,
    source_path: String,
}

pub(super) struct MemoryState {
    /// store 已排好序:项目路径分组、组内新到旧、用户级最后;行里**不带正文**(列表列
    /// 给的是空串),选中时按 key 另取
    docs: Vec<MemoryDoc>,
    /// 组头 + 文档扁平成行,交 gpui::list 虚拟化——只画看得见的几行,行文案在画
    /// 的时候现算(译文与相对时间都不缓存,换语言不会留旧串)
    rows: Vec<MemoryRow>,
    list: gpui::ListState,
    loading: bool,
    selected: Option<String>,
    /// 选中后后台读,读到前是 None(转圈)
    loaded: Option<LoadedMemory>,
    /// 侧栏导航的计数(All Memory / Agents / Projects),与 docs 同一次查询刷新
    counts: MemoryCounts,
    /// 侧栏筛选(单选,与会话侧栏同一模型):agent 或项目,`Some("")` 是没归属的一组
    agent: Option<AgentId>,
    project: Option<String>,
    /// 筛选刚换过(进页归零、点了导航行):下一次重载必须查(扫描进行中也不按住)并把
    /// 列表拉回顶部、不恢复旧滚动位置
    scope_changed: bool,
    /// 进行中的列表/正文任务;新任务覆盖旧值即取消
    load_task: Option<Task<()>>,
    body_task: Option<Task<()>>,
}

impl Default for MemoryState {
    fn default() -> Self {
        Self {
            docs: Vec::new(),
            rows: Vec::new(),
            list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(200.)),
            loading: false,
            selected: None,
            loaded: None,
            counts: MemoryCounts::default(),
            agent: None,
            project: None,
            scope_changed: false,
            load_task: None,
            body_task: None,
        }
    }
}

impl Workbench {
    /// 进 Memory 页时从"全部"开始
    pub(super) fn reset_memory_scope(&mut self) {
        self.memory.agent = None;
        self.memory.project = None;
        self.memory.scope_changed = true;
    }

    /// 侧栏导航行的唯一写入点(agent 与项目互斥,与会话侧栏的 set_scope 同形)
    fn set_memory_scope(
        &mut self,
        agent: Option<AgentId>,
        project: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.memory.agent = agent;
        self.memory.project = project;
        self.memory.scope_changed = true;
        self.reload_memories(cx);
    }

    /// refresh 顺带重载(与 reload_insights 同规矩):扫描进行中且已有数据就按住,
    /// 终态 Progress 补最后一次——**筛选刚换过除外**:这里也是侧栏导航的唯一落点,
    /// 按住就是点了没反应、标题换了列表没换(2026-09-21 review)。选中项还在就保留,
    /// 否则落到第一份
    pub(super) fn reload_memories(&mut self, cx: &mut Context<Self>) {
        if self.page != Page::Memory {
            return;
        }
        if self.scan.scanning && !self.memory.docs.is_empty() && !self.memory.scope_changed {
            return;
        }
        self.memory.loading = self.memory.docs.is_empty();
        let store = self.store.clone();
        // 侧栏的 "Unknown project" 行是 `Some("")`:筛的是没归属的一组,不是路径为空串的项目
        let filter = MemoryFilter {
            agents: self.memory.agent.into_iter().collect(),
            project_paths: self
                .memory
                .project
                .iter()
                .filter(|p| !p.is_empty())
                .cloned()
                .collect(),
            unattributed: self.memory.project.as_deref() == Some(""),
            limit: 0,
        };
        let task = cx.background_spawn(async move {
            let docs = store.list_memories(&filter)?;
            let counts = store.memory_counts()?;
            anyhow::Ok((docs, counts))
        });
        self.memory.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = task.await;
            this.update(cx, |this, cx| {
                if let Ok((docs, counts)) = loaded {
                    this.memory.counts = counts;
                    let rows = memory_rows(&docs);
                    // 重载来自每轮扫描,滚动位置要留住:splice 整段会把落在段内的
                    // 滚动锚点归零(gpui 的 splice 只保段外的锚点),所以先记下再恢复;
                    // 筛选刚换过则回到顶部
                    let top = if this.memory.scope_changed {
                        gpui::ListOffset {
                            item_ix: 0,
                            offset_in_item: px(0.),
                        }
                    } else {
                        this.memory.list.logical_scroll_top()
                    };
                    this.memory.scope_changed = false;
                    let old = this.memory.rows.len();
                    this.memory.list.splice(0..old, rows.len());
                    this.memory.list.scroll_to(top);
                    // 选中项:还在且没变就不动;正文被 agent 改过(时间或大小变了)
                    // 重读,否则新标题配旧正文;没了就落到第一份
                    let stamp = |docs: &[MemoryDoc], key: &str| {
                        docs.iter()
                            .find(|d| d.key == key)
                            .map(|d| (d.updated_at, d.size_bytes))
                    };
                    let selected = this.memory.selected.clone();
                    let before = selected
                        .as_deref()
                        .and_then(|k| stamp(&this.memory.docs, k));
                    let after = selected.as_deref().and_then(|k| stamp(&docs, k));
                    this.memory.docs = docs;
                    this.memory.rows = rows;
                    match (selected, after) {
                        (Some(key), Some(now)) if before != Some(now) => {
                            this.select_memory(key, cx);
                        }
                        (Some(_), Some(_)) => {}
                        _ => {
                            this.memory.selected = None;
                            this.memory.loaded = None;
                            if let Some(first) = this.memory.docs.first().map(|d| d.key.clone()) {
                                this.select_memory(first, cx);
                            }
                        }
                    }
                }
                this.memory.loading = false;
                cx.notify();
            })
            .ok();
        }));
    }

    fn select_memory(&mut self, key: String, cx: &mut Context<Self>) {
        let Some(path) = self
            .memory
            .docs
            .iter()
            .find(|d| d.key == key)
            .map(|d| d.path.clone())
        else {
            return;
        };
        self.memory.selected = Some(key.clone());
        self.memory.loaded = None;
        // 列表行不带正文,选中时按 key 另取一行;正文与来源路径都在后台算:两者都要
        // stat(虚拟路径解析、现场读文件),自定义 location 挂在慢盘上时不能卡 UI 线程
        let store = self.store.clone();
        let task = {
            let key = key.clone();
            cx.background_spawn(async move {
                let doc = store
                    .get_memory(&key)?
                    .ok_or_else(|| anyhow::anyhow!("memory {key} is no longer indexed"))?;
                anyhow::Ok(LoadedMemory {
                    source_path: session_source_path(&doc.path).to_string(),
                    body: memory_body(&doc).into(),
                })
            })
        };
        self.memory.body_task = Some(cx.spawn(async move |this, cx| {
            let loaded = task.await;
            this.update(cx, |this, cx| {
                // 只写回仍然选中的那份,快速切换时旧任务不覆盖新选择
                if this.memory.selected.as_deref() != Some(key.as_str()) {
                    return;
                }
                // 读不出(刚被扫描清掉、库读错)也得收场,不能让阅读面一直转圈;来源路径
                // 同样要解析过(虚拟路径落到库文件),Copy path 才不会给出 `<db>#<id>`
                this.memory.loaded = Some(loaded.unwrap_or_else(|e| LoadedMemory {
                    source_path: session_source_path(&path).to_string(),
                    body:
                        crate::tf!("Could not read this memory file: {}", format!("{e:#}")).into(),
                }));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 会话页同一形制(用户 2026-09-21:整页宽的页头压着两栏"看着很奇怪"):左列 =
    /// 列内页头 + 记忆流,右侧 = 阅读面(详情页同款头部 + popover 正文)
    pub(super) fn render_memory(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let count = self.memory.docs.len();
        let subtitle: SharedString = crate::tp!("{} memory file", "{} memory files", count).into();
        let column: AnyElement = if self.memory.docs.is_empty() {
            div().flex_1().into_any_element()
        } else {
            self.render_memory_list(cx)
        };
        // 列表非空时选中项恒有(reload 落到第一份),阅读面跟着它;空库给空态卡
        let selected = self
            .memory
            .selected
            .as_ref()
            .and_then(|k| self.memory.docs.iter().find(|d| &d.key == k));
        let pane: AnyElement = match selected {
            Some(doc) => self.render_memory_reader(doc, cx),
            None => v_flex()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .bg(theme.background)
                .child(if self.memory.loading {
                    Spinner::new().into_any_element()
                } else {
                    empty_state_card(
                        "icons/file-text.svg",
                        px(58.),
                        px(24.),
                        t("No memory files yet"),
                        t("Agents write these as they work: Claude Code's auto-memory, Codex's memories, ZCode's project memory. They show up here once they exist."),
                        cx,
                    )
                    .into_any_element()
                })
                .into_any_element(),
        };
        h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                v_flex()
                    .w(SESSION_STREAM_WIDTH)
                    .h_full()
                    .flex_shrink_0()
                    .bg(theme.colors.list)
                    .child(library_header(
                        "memory-header",
                        self.memory_context_title(),
                        subtitle,
                        SPACE_LG,
                        Some(self.refresh_button(cx)),
                        cx,
                    ))
                    .child(column),
            )
            .child(pane)
            .into_any_element()
    }

    /// 左列页头的标题 = 当前侧栏筛选(与会话页 context_title 同规矩)
    fn memory_context_title(&self) -> String {
        if let Some(agent) = self.memory.agent {
            return agent.display_name().to_string();
        }
        if let Some(path) = &self.memory.project {
            if path.is_empty() {
                return t("Unknown project").to_string();
            }
            return self
                .memory
                .counts
                .projects
                .iter()
                .find(|p| &p.path == path)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| t("Projects").to_string());
        }
        t("All Memory").to_string()
    }

    /// 记忆流:gpui::list 虚拟化(行高不等:分组头与两行的文档行)
    fn render_memory_list(&self, cx: &Context<Self>) -> AnyElement {
        let entity = cx.entity().downgrade();
        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .child(
                gpui::list(self.memory.list.clone(), move |ix, _window, cx| {
                    entity
                        .upgrade()
                        .map(|e| e.update(cx, |this, cx| this.render_memory_row(ix, cx)))
                        .unwrap_or_else(|| div().into_any_element())
                })
                .size_full(),
            )
            .vertical_scrollbar(&self.memory.list)
            .into_any_element()
    }

    /// 一行:分组头(会话流的时间分割线同款),或两行的文档行(会话行同款:Body 14
    /// medium 标题一行;Label 11 的品牌图 + 归属徽章 + 右对齐时间)
    fn render_memory_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(row) = self.memory.rows.get(ix).copied() else {
            return div().into_any_element();
        };
        match row {
            MemoryRow::Group(first) => {
                // 组头 = 会话流的时间分割线(用户 2026-09-21 定)
                let label = self
                    .memory
                    .docs
                    .get(first)
                    .map(memory_group_label)
                    .unwrap_or_default();
                section_header_row(label, &theme).into_any_element()
            }
            MemoryRow::Doc(di) => {
                let Some(d) = self.memory.docs.get(di) else {
                    return div().into_any_element();
                };
                let selected = self.memory.selected.as_deref() == Some(d.key.as_str());
                let key = d.key.clone();
                let title: SharedString = clip_display(&d.title, SESSION_TITLE_MAX_WIDTH).into();
                let title_tooltip: SharedString = d.title.clone().into();
                let shown_time: SharedString = smart_time(d.updated_at).into();
                let shown_tooltip: SharedString = abs_date(d.updated_at).into();
                // 盒模型逐项照会话行:ListItem 自带 px_3 / py_1(12 / 4)再套行内容的
                // SPACE_XS / SPACE_SM(4 / 8),高亮面 mx = SPACE_SM——这里没有 ListItem,
                // 把两层加成一层写死同样的数。不缩进、不画竖线(树的子行形制试过,用户
                // 2026-09-21 定没必要)
                div()
                    .w_full()
                    .px(SPACE_SM)
                    .child(
                        v_flex()
                            .id(("memory-row", ix))
                            .w_full()
                            .min_w_0()
                            .rounded(theme.radius)
                            .px(SPACE_MD + SPACE_XS)
                            .py(SPACE_XS + SPACE_SM)
                            .gap(SPACE_XS)
                            .cursor_pointer()
                            .when(selected, |row| row.bg(theme.list_active))
                            .when(!selected, |row| {
                                row.hover(|style| style.bg(theme.list_hover))
                            })
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.select_memory(key.clone(), cx)
                            }))
                            .child(
                                div()
                                    .id(("memory-title", ix))
                                    .w_full()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_size(FONT_BODY)
                                    .font_medium()
                                    .text_color(theme.foreground)
                                    .child(title)
                                    .tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(title_tooltip.clone())
                                            .build(window, cx)
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .text_size(FONT_LABEL)
                                    .text_color(theme.muted_foreground)
                                    // 项目名由组头说明,行里不重复挂项目徽章
                                    .child(
                                        img(d.agent.brand_icon(theme.mode.is_dark()))
                                            .size(px(15.))
                                            .flex_shrink_0(),
                                    )
                                    .children(memory_badges(d, &theme))
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .id(("memory-time", ix))
                                            .flex_shrink_0()
                                            .child(shown_time)
                                            .tooltip(move |window, cx| {
                                                gpui_component::tooltip::Tooltip::new(
                                                    shown_tooltip.clone(),
                                                )
                                                .build(window, cx)
                                            }),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    /// 阅读面,详情页同款头部(`detail_header_frame`):上下文行是品牌图 + agent + 归属
    /// 徽章,右端 Reveal / Copy path;元信息是文件路径与更新时间;正文 popover 底、
    /// 720 阅读宽居中,与消息正文同一套 Markdown 渲染
    fn render_memory_reader(&self, doc: &MemoryDoc, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let dark = theme.mode.is_dark();
        // SQLite 型的虚拟路径落到库文件本体,Reveal 与 Copy 才有东西可指;解析在
        // select_memory 的后台任务里做好,读到之前按 scope 剥虚拟后缀顶一下(不 stat)
        let source_path = self
            .memory
            .loaded
            .as_ref()
            .map(|l| l.source_path.clone())
            .unwrap_or_else(|| memory_display_path(doc));
        let reveal_path = source_path.clone();
        let copy_path = source_path.clone();
        // 相对链接按记忆文件所在目录解析(MEMORY.md 链接的是同目录的主题文件),
        // 不是项目根;SQLite 型没有目录,退项目路径
        let link_base = if source_path == doc.path {
            std::path::Path::new(&doc.path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            doc.project_path.clone()
        };
        let updated: SharedString = crate::tf!("Updated {}", smart_time(doc.updated_at)).into();
        let updated_tooltip: SharedString =
            crate::tf!("Updated {}", abs_date(doc.updated_at)).into();
        let mut lead: Vec<AnyElement> = vec![
            img(doc.agent.brand_icon(dark))
                .size(px(15.))
                .flex_shrink_0()
                .into_any_element(),
            div()
                .flex_shrink_0()
                .child(doc.agent.display_name())
                .into_any_element(),
            project_badge(
                "memory-project",
                &doc.project_path,
                memory_group_label(doc),
                &theme,
            ),
        ];
        lead.extend(memory_badges(doc, &theme));
        let actions: Vec<AnyElement> = vec![
            Button::new("memory-reveal")
                .ghost()
                .rounded(RADIUS_BUTTON)
                .icon(icon("icons/folder.svg").with_size(px(16.)))
                .tooltip(reveal_in_fm())
                .on_click(move |_, _, _| {
                    terminal::reveal_in_file_manager(&reveal_path);
                })
                .into_any_element(),
            Button::new("memory-copy-path")
                .ghost()
                .rounded(RADIUS_BUTTON)
                .icon(icon("icons/copy.svg").with_size(px(16.)))
                .tooltip(t("Copy path"))
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                })
                .into_any_element(),
        ];
        let meta_rows: Vec<AnyElement> = vec![
            h_flex()
                .min_w_0()
                .gap(px(6.))
                .child(
                    icon("icons/file-text.svg")
                        .with_size(px(12.))
                        .flex_shrink_0(),
                )
                .child(div().min_w_0().truncate().child(tilde_path(&source_path)))
                .into_any_element(),
            h_flex()
                .min_w_0()
                .gap(px(6.))
                .items_center()
                .child(
                    icon("icons/calendar.svg")
                        .with_size(px(12.))
                        .flex_shrink_0(),
                )
                .child(
                    div()
                        .id("memory-updated")
                        .min_w_0()
                        .truncate()
                        .child(updated)
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(updated_tooltip.clone())
                                .build(window, cx)
                        }),
                )
                .into_any_element(),
        ];
        let header = detail_header_frame(
            "memory-detail-header",
            lead,
            actions,
            doc.title.clone().into(),
            meta_rows,
            &theme,
        );
        let content: AnyElement = match &self.memory.loaded {
            Some(loaded) => markdown_body(
                format!("memory-{}", doc.key).into(),
                loaded.body.clone(),
                &doc.host,
                &link_base,
                FONT_MSG_BODY,
                gpui::rems(0.5),
                dark,
                cx,
            )
            .into_any_element(),
            None => div()
                .w_full()
                .flex()
                .justify_center()
                .py(SPACE_XXL)
                .child(Spinner::new())
                .into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(header)
            .child(
                div()
                    .id("memory-reader")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .bg(theme.popover)
                    // 与详情正文同一组数:24px 阅读轴,首条上留 16、末条下留 24
                    .px(SPACE_XXL)
                    .pt(SPACE_LG)
                    .pb(SPACE_XXL)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .child(div().w_full().max_w(READER_MAX_WIDTH).child(content)),
                    ),
            )
            .into_any_element()
    }

    /// Memory 页的侧栏导航,替换会话导航(搜索框、All Sessions / Starred、Agents /
    /// Projects):All Memory 一行,再接 Agents / Projects 两组(`sidebar_groups`,与会话
    /// 侧栏同一套组件、同一个单选模型)按记忆文件计数
    pub(super) fn render_memory_nav(&self, cx: &Context<Self>) -> AnyElement {
        let counts = &self.memory.counts;
        let all_active = self.memory.agent.is_none() && self.memory.project.is_none();
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px(SIDEBAR_EDGE)
                    .pb(SPACE_XS)
                    .gap(SPACE_XS)
                    // 不放模式标题:当前页由底部工具条点亮的那颗说明(曾加过一行
                    // brain + "Memory",用户 2026-09-21 定没必要)
                    .child(sidebar_row(
                        "memory-all",
                        RowLead::Icon(icon("icons/file-text.svg")),
                        t("All Memory"),
                        Some(counts.total),
                        all_active,
                        RowLevel::Primary,
                        cx.listener(|this, _, _window, cx| {
                            this.set_memory_scope(None, None, cx);
                        }),
                        cx,
                    )),
            )
            .child(
                self.sidebar_groups(
                    "memory-sidebar-scroll",
                    counts
                        .agents
                        .iter()
                        .map(|(agent, count)| (*agent, *count, self.memory.agent == Some(*agent))),
                    counts.projects.iter().map(|p| {
                        let label: SharedString = if p.path.is_empty() {
                            t("Unknown project").into()
                        } else {
                            p.name.clone().into()
                        };
                        (
                            p.path.clone(),
                            label,
                            p.count,
                            self.memory.project.as_deref() == Some(p.path.as_str()),
                        )
                    }),
                    |this, next, cx| this.set_memory_scope(next, None, cx),
                    |this, next, cx| this.set_memory_scope(None, next, cx),
                    cx,
                ),
            )
            .into_any_element()
    }
}

/// 组头 + 文档扁平成行;分组判据是 `MemoryDoc::group`(与 store 的排序一致:项目按
/// 路径、没归属的一组在项目之后、用户级最后)。不折叠、不缩进(树的形制试过,用户
/// 2026-09-21 定没必要)
fn memory_rows(docs: &[MemoryDoc]) -> Vec<MemoryRow> {
    let mut rows = Vec::with_capacity(docs.len() + 8);
    let mut current: Option<MemoryGroup> = None;
    for (ix, d) in docs.iter().enumerate() {
        let group = d.group();
        if current != Some(group) {
            rows.push(MemoryRow::Group(ix));
            current = Some(group);
        }
        rows.push(MemoryRow::Doc(ix));
    }
    rows
}

/// 读到 LoadedMemory 之前阅读面要显示的来源路径:线程记忆(Codex 的逐会话摘要)的
/// 虚拟路径 `<db>#<id>` 只剥后缀、不 stat(render 每帧调);文件型就是它自己
fn memory_display_path(d: &MemoryDoc) -> String {
    if d.scope == MemoryScope::Thread {
        d.path
            .rsplit_once('#')
            .map_or_else(|| d.path.clone(), |(db, _)| db.to_string())
    } else {
        d.path.clone()
    }
}

/// 组头 / 归属徽章的文案:项目名;用户级记忆一组、没有项目的一组
fn memory_group_label(d: &MemoryDoc) -> SharedString {
    match d.group() {
        MemoryGroup::User => t("User-level").into(),
        MemoryGroup::Unknown => t("Unknown project").into(),
        MemoryGroup::Project { name, .. } => name.to_string().into(),
    }
}

/// 文档行与阅读面头部共用的两枚徽章(有则画):Codex 逐会话记忆的 "session memory",
/// 远程记忆的 @host
fn memory_badges(d: &MemoryDoc, theme: &gpui_component::Theme) -> Vec<AnyElement> {
    let mut out = Vec::with_capacity(2);
    if d.scope == MemoryScope::Thread {
        out.push(
            badge(t("session memory"), theme.muted, theme.muted_foreground).into_any_element(),
        );
    }
    if !d.host.is_empty() {
        out.push(host_badge(&d.host, theme).into_any_element());
    }
    out
}
