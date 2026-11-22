//! `schedule_*`: 定时任务管理工具组。
//!
//! 工具本身只校验参数、解析规则并格式化结果；真实的管理服务经 `ToolContext`
//! 注入的 [`SchedulerHandle`]（GUI 引擎把运行时 `Scheduler` 传进来）。子代理 /
//! CLI / ACP 等没有运行时驱动的上下文里没有该服务，工具返回"服务不可用"错误
//! 而不是静默失败——所以它们也从子代理注册表里被显式剔除（见 `ToolRegistry::without`）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qcode_core::provider::ToolSpec;
use qcode_core::question::{
    AnswerItem, QuestionAnswer, QuestionIntent, QuestionItem, QuestionOption, QuestionRequest,
};
use qcode_core::schedule::{
    MIN_DELIVERY_INTERVAL, Overlap, ScheduledTask, Target, now_local, parse_rule,
};
use qcode_core::scheduler::{EditTask, NewTask, SchedulerHandle};
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

/// 没有运行时驱动的会话：定时任务工具不可用。
///
/// ADR-0088 之后这条只覆盖「连 store 都没有」的上下文（近乎不存在）：
/// 能不能**登记**看 store，能不能**触发**看 [`qcode_core::scheduler::DriverStatus`]，
/// 两件事不再由同一个 Option 回答。
/// 四个工具共用。改成函数：译文来自运行期 locale，不能是 `const`。
fn unavailable() -> String {
    qcode_i18n::tr("tool.schedule.unavailable").to_string()
}

/// 四个定时任务工具的名字，供子代理注册表剔除。
pub const SCHEDULE_TOOL_NAMES: [&str; 4] = [
    "schedule_create",
    "schedule_list",
    "schedule_view",
    "schedule_edit",
];

fn handle(ctx: &ToolContext) -> Option<Arc<dyn SchedulerHandle>> {
    ctx.scheduler.clone()
}

/// `target` 参数：`new_once`（默认）/ `new` / `session:<id>`，以及提问选项的
/// 中文 label（`复用同一个会话` / `每次新建一个会话`）——按钮显示中文，模型
/// 收到的答案就是 label 本身。
fn parse_target(text: &str) -> Option<Target> {
    let text = text.trim();
    // 选项 label 兼作解析输入：用户点按钮，模型看到的是 label 文本，再原样传回来。
    // 所以这里**必须**用同一对 key 取当前语言的 label，不能写死中文——否则
    // 英文界面上点「Reuse one session」回来的是英文，比较却对着中文，直接解析失败。
    let reuse = qcode_i18n::tr("tool.schedule.ask.target_reuse_label");
    let fresh = qcode_i18n::tr("tool.schedule.ask.target_new_label");
    if text.is_empty() || text == "new_once" || text == "new-once" || text == reuse {
        return Some(Target::NewOnce);
    }
    if text == "new" || text == fresh {
        return Some(Target::New);
    }
    if let Some(id) = text.strip_prefix("session:") {
        let id = id.trim();
        return (!id.is_empty()).then(|| Target::Session { id: id.to_string() });
    }
    None
}

/// 下次触发时刻的展示：`None` → `-`。
fn fire_display(task: &ScheduledTask) -> String {
    task.next_fire_at
        .map_or_else(|| "-".to_string(), |at| format!("{at}"))
}

/// 列表里一行任务的紧凑描述。
fn row(task: &ScheduledTask) -> String {
    qcode_i18n::trp(
        "core.schedule.row",
        &[
            task.status.as_str(),
            task.title.as_str(),
            task.id.as_str(),
            task.rule.describe().as_str(),
            task.target.describe().as_str(),
            fire_display(task).as_str(),
        ],
    )
}

/// 单个任务的详情。
fn detail(task: &ScheduledTask) -> String {
    let mut lines = vec![
        qcode_i18n::trp(
            "core.schedule.field.title",
            &[task.title.as_str(), task.id.as_str()],
        ),
        qcode_i18n::trp("core.schedule.field.status", &[task.status.as_str()]),
        qcode_i18n::trp("core.schedule.field.rule", &[task.rule.describe().as_str()]),
        qcode_i18n::trp(
            "core.schedule.field.target",
            &[task.target.describe().as_str()],
        ),
        qcode_i18n::trp(
            "core.schedule.field.next_fire",
            &[fire_display(task).as_str()],
        ),
        qcode_i18n::trp(
            "core.schedule.field.min_interval",
            &[MIN_DELIVERY_INTERVAL.whole_seconds().to_string()],
        ),
    ];
    // ADR-0092 §7：不设截止时间**不是**「无所谓」，是「有人值守时一直等、
    // 无人值守时补缺省值」。省略这一行会让模型以为它是无限期，然后建出一条
    // 在 daemon 上会被自动拒绝的任务而毫不知情。设了值也走同一句话，
    // 免得同一份报告里出现两种措辞。
    lines.push(qcode_core::scheduler::approval_footer(
        task.approval_timeout,
    ));
    if let Some(cwd) = &task.cwd {
        lines.push(qcode_i18n::trp("core.schedule.field.cwd", &[cwd.as_str()]));
    }
    if let Some(session) = &task.reserved_session_id {
        lines.push(qcode_i18n::trp(
            "core.schedule.field.reserved_session",
            &[session.as_str()],
        ));
    }
    if let Some(receipt) = &task.last_receipt {
        lines.push(qcode_i18n::trp(
            "core.schedule.field.last_delivery",
            &[
                receipt.occurrence_at.to_string(),
                receipt
                    .target_session_id
                    .as_deref()
                    .unwrap_or("-")
                    .to_string(),
                receipt.message_id.to_string(),
            ],
        ));
    }
    let count = task.delivery_log.len();
    if task.missed_windows > 0 {
        lines.push(qcode_i18n::trp(
            "core.schedule.field.missed",
            &[task.missed_windows.to_string()],
        ));
    }
    lines.push(qcode_i18n::trp(
        "core.schedule.field.runs",
        &[count.to_string()],
    ));
    for receipt in task.delivery_log.iter().rev().take(5) {
        lines.push(qcode_i18n::trp(
            "core.schedule.field.delivery_line",
            &[
                receipt.delivered_at.to_string(),
                receipt
                    .target_session_id
                    .as_deref()
                    .unwrap_or("-")
                    .to_string(),
                receipt.message_id.to_string(),
                receipt.prompt.clone(),
            ],
        ));
    }
    if task.earlier_records_pruned {
        lines.push(qcode_i18n::tr("core.schedule.field.pruned").to_string());
    }
    lines.join("\n")
}

/// `schedule_create`: 按模型给的描述创建一条定时任务。
pub struct ScheduleCreate;

#[derive(Deserialize)]
struct CreateArgs {
    title: Option<String>,
    prompt: Option<String>,
    rule: Option<String>,
    #[serde(default)]
    target: String,
    #[serde(default)]
    overlap: String,
    approval_timeout_secs: Option<u64>,
    /// 投递回合的工作目录（绝对路径）。`None` = 用会话 / 引擎默认。
    cwd: Option<String>,
}

#[async_trait]
impl Tool for ScheduleCreate {
    fn name(&self) -> &str {
        "schedule_create"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: concat!(
                "Create a scheduled task. At the next trigger time the prompt is delivered ",
                "as a message into a conversation (the target), then the task runs again ",
                "per its rule. Rules: `cron <expr>` (5 fields, local time), ",
                "`once YYYY-MM-DD HH:MM`, `every <seconds>` (>= 60), `daily HH:MM`, ",
                "`weekly <weekdays> HH:MM` (weekdays may repeat: 0 or sun = Sunday, ",
                "1..7 or mon..sat; comma- or space-separated, e.g. `1,3,5`), ",
                "`monthly <day> HH:MM` (day 1-31). ",
                "Fields the user has not stated are collected by the tool itself: it ",
                "pauses and asks the user for the missing name, prompt, rule, working ",
                "directory and delivery target in one round — never invent a default ",
                "schedule such as \"every day at 08:00\". The delivery target is exactly ",
                "two options: reuse one session for every delivery (in a conversation ",
                "that session is the current one), or create a new session each time. ",
                "The working directory (`cwd`) is where the delivered turn runs: use an ",
                "absolute path the user named; when it is not given the tool asks for ",
                "it too (the user may skip it and fall back to the session default)."
            )
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "A short task name in the user's own language, shaped as \"action + object\" (e.g. \"fetch Beijing time\"). Leave out meta words like \"create\"/\"scheduled task\", and leave out time, status and id. The tool asks the user when it is missing; may be omitted."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The reminder delivered into the conversation at trigger time (equivalent to one user message). The tool asks the user when it is missing; may be omitted."
                    },
                    "rule": {
                        "type": "string",
                        "description": "Trigger rule: cron <expr> / once YYYY-MM-DD HH:MM / every <seconds> (>= 60) / daily HH:MM / weekly <weekdays> HH:MM (may repeat, comma-separated, e.g. 1,3,5; 0 or sun = Sunday) / monthly <day> HH:MM (day 1-31). The tool asks the user when it is missing (listing every period form); may be omitted."
                    },
                    "target": {
                        "type": "string",
                        "description": "Delivery target, one of two: new_once = reuse one session for every delivery (default; in a conversation, the current one); new = create a new session each time. Use session:<session-id> only when the user names a specific conversation. The tool asks the user when it is missing; may be omitted."
                    },
                    "overlap": {
                        "type": "string",
                        "enum": ["queue", "skip"],
                        "description": "When the target session is busy: queue = wait for the turn to end (default), skip = skip this delivery."
                    },
                    "approval_timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Approval timeout in seconds for the delivered turn; omit to wait for human approval indefinitely."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory of the delivered turn (absolute path). Use it when the user gave one; when unsure, omit it and the tool asks (the user may skip and use the default)."
                    }
                },
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        // 只写会话外部的任务定义（任务存储），无文件 / 进程 / 网络副作用。
        ApprovalClass::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let Some(scheduler) = handle(ctx) else {
            return Ok(ToolOutput::error(unavailable()));
        };
        let args: CreateArgs =
            serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))?;

        // 缺失项一次问全：名称 / 内容 / 周期 / 工作目录 / 目标。不依赖模型自觉
        // 先调 ask_user——描述里的引导可能被模型忽略，这里由工具自己问。
        let mut title = trim_opt(args.title);
        let mut prompt = trim_opt(args.prompt);
        let mut rule_text = trim_opt(args.rule);
        let mut cwd = args
            .cwd
            .map(|text| text.trim().to_string())
            .filter(|t| !t.is_empty());
        let mut target = trim_opt(Some(args.target));

        let mut questions: Vec<QuestionItem> = Vec::new();
        if title.is_none() {
            questions.push(free_question(
                "title",
                qcode_i18n::tr("tool.schedule.ask.title").as_ref(),
            ));
        }
        if prompt.is_none() {
            questions.push(free_question(
                "prompt",
                qcode_i18n::tr("tool.schedule.ask.prompt").as_ref(),
            ));
        }
        if rule_text.is_none() {
            questions.push(QuestionItem {
                id: "rule".to_string(),
                header: None,
                question: qcode_i18n::tr("tool.schedule.ask.rule").to_string(),
                detail: Some(qcode_i18n::tr("tool.schedule.ask.rule_detail").to_string()),
                options: Vec::new(),
                multi_select: false,
                pick_directory: false,
                schedule_rule: true,
            });
        }
        // cwd / target 有默认（本回合工作目录 / new_once），只在「有人可问」时才
        // 问；headless（`qc run --yes`、子代理）下直接回落默认，不制造提问。
        if ctx.questioner.is_some() && cwd.is_none() {
            questions.push(QuestionItem {
                id: "cwd".to_string(),
                header: None,
                question: qcode_i18n::tr("tool.schedule.ask.cwd").to_string(),
                detail: None,
                options: Vec::new(),
                multi_select: false,
                pick_directory: true,
                schedule_rule: false,
            });
        }
        if ctx.questioner.is_some() && target.is_none() {
            questions.push(QuestionItem {
                id: "target".to_string(),
                header: None,
                question: qcode_i18n::tr("tool.schedule.ask.target").to_string(),
                detail: Some(qcode_i18n::tr("tool.schedule.ask.target_detail").to_string()),
                options: vec![
                    QuestionOption {
                        label: qcode_i18n::tr("tool.schedule.ask.target_reuse_label").to_string(),
                        description: Some(
                            qcode_i18n::tr("tool.schedule.ask.target_reuse_desc").to_string(),
                        ),
                    },
                    QuestionOption {
                        label: qcode_i18n::tr("tool.schedule.ask.target_new_label").to_string(),
                        description: Some(
                            qcode_i18n::tr("tool.schedule.ask.target_new_desc").to_string(),
                        ),
                    },
                ],
                multi_select: false,
                pick_directory: false,
                schedule_rule: false,
            });
        }

        if !questions.is_empty() {
            let Some(questioner) = &ctx.questioner else {
                // headless（`qc run --yes`、子代理）没有提问方：cwd / target 的
                // 问题根本不会进列表，走到这里缺的只可能是必填三项。不能凭空
                // 编造，明确告诉模型它需要先向用户确认。
                return Ok(ToolOutput::error(
                    qcode_i18n::tr("tool.schedule.error.missing_info").to_string(),
                ));
            };
            let request = QuestionRequest {
                call_id: ctx.call_id.clone(),
                questions,
                intent: QuestionIntent::Confirm,
                timeout: None,
            };
            match questioner.ask(&request, ctx.cancel.clone()).await {
                QuestionAnswer::Answered(answers) => {
                    if title.is_none() {
                        title = answer_text(&answers, "title").map(str::to_string);
                    }
                    if prompt.is_none() {
                        prompt = answer_text(&answers, "prompt").map(str::to_string);
                    }
                    if rule_text.is_none() {
                        rule_text = answer_text(&answers, "rule").map(str::to_string);
                    }
                    if cwd.is_none() {
                        cwd = answer_text(&answers, "cwd").map(str::to_string);
                    }
                    if target.is_none() {
                        target = answer_choice(&answers, "target").map(str::to_string);
                    }
                }
                // 超时 / 取消：信息不齐，不创建。
                _ => {
                    return Ok(ToolOutput::error(
                        qcode_i18n::tr("tool.schedule.error.cancelled").to_string(),
                    ));
                }
            }
        }

        // 必填项：问过仍缺（用户跳过）就不能继续。
        let Some(title) = title else {
            return Ok(ToolOutput::error(
                qcode_i18n::tr("tool.schedule.error.title_required").to_string(),
            ));
        };
        let Some(prompt) = prompt else {
            return Ok(ToolOutput::error(
                qcode_i18n::tr("tool.schedule.error.prompt_required").to_string(),
            ));
        };
        let Some(rule_text) = rule_text else {
            return Ok(ToolOutput::error(
                qcode_i18n::tr("tool.schedule.error.rule_required").to_string(),
            ));
        };
        // 规则 / 目标 / overlap 的解析错误是模型可以修正的输入错误，作为工具
        // 结果返回（todo 工具同款约定），而不是引擎级失败。
        let rule = match parse_rule(&rule_text) {
            Ok(rule) => rule,
            Err(error) => {
                return Ok(ToolOutput::error(qcode_i18n::trp(
                    "tool.schedule.error.invalid_rule",
                    &[error.to_string()],
                )));
            }
        };
        let target = match target {
            Some(text) => match parse_target(&text) {
                Some(target) => target,
                None => {
                    return Ok(ToolOutput::error(
                        qcode_i18n::tr("tool.schedule.error.bad_target").to_string(),
                    ));
                }
            },
            None => Target::NewOnce,
        };
        // 在会话内创建且选「每次复用会话」（new_once）时，复用当前对话——这样
        // 任务从创建起就绑定到会话，详情页可立即跳转。
        let target = match target {
            Target::NewOnce if !ctx.session_id.is_empty() => Target::Session {
                id: ctx.session_id.clone(),
            },
            other => other,
        };
        let overlap = if args.overlap.is_empty() {
            Overlap::Queue
        } else {
            match Overlap::parse(&args.overlap) {
                Some(overlap) => overlap,
                None => {
                    return Ok(ToolOutput::error(
                        qcode_i18n::tr("tool.schedule.error.bad_overlap").to_string(),
                    ));
                }
            }
        };
        let approval_timeout = args.approval_timeout_secs.map(Duration::from_secs);

        // 兜底成这个回合的工作目录（ADR-0090 §11）。
        //
        // 上面那道「问用户」在 headless 下不存在：`qc run` 只在「非 --yes 且有 tty」
        // 时才装 questioner，其余情况是 `None`（`commands/run.rs:224-228`）。于是
        // `cwd` 留在 `None`，任务到点回落到驱动进程的 workspace——一个用户看不见的
        // 目录。「在仓库里建的每日任务实际在家目录里跑」就是这么来的。
        //
        // 不另立规则：`shell_cwd()` 给出的正是本回合的 workspace，也就是 ADR-0090
        // §2 那条「CLI 用 pwd / GUI 用启动目录」的答案。这里只是把它从「隐式回落」
        // 变成「写进任务」。也不必再判一次文件系统根：沙箱拒绝 `/` 作为 root，
        // `/` 根本走不到 tool 这一步。
        let cwd = cwd.or_else(|| {
            let resolved = ctx
                .shell_cwd()
                .and_then(|path| std::path::absolute(&path).ok());
            if resolved.is_none() {
                tracing::warn!(
                    "schedule_create: no working directory to fall back on; leaving cwd unset"
                );
            }
            resolved.map(|path| path.display().to_string())
        });

        let task = scheduler
            .create(
                NewTask {
                    title,
                    prompt,
                    rule,
                    target,
                    overlap,
                    approval_timeout,
                    cwd,
                },
                now_local(),
            )
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        // 绑定到会话（每次复用当前会话）且该会话没有显式标题时，把任务名作为
        // 会话标题：对话没起名，任务名就是对话名（GUI/TUI/CLI 统一生效）。
        if let Some(session) = task.target.session_id()
            && !session.is_empty()
            && let Err(err) = scheduler.set_session_title_if_missing(session, &task.title)
        {
            // 写标题失败不否决创建：任务已经登记成功，标题只是锦上添花。
            tracing::warn!(%err, session, "schedule_create: naming the bound session failed");
        }
        Ok(ToolOutput::ok(qcode_i18n::trp(
            "tool.schedule.output.created",
            &[
                task.title.as_str(),
                task.id.as_str(),
                detail(&task).as_str(),
                qcode_core::scheduler::driver_footer(ctx.driver).as_str(),
            ],
        )))
    }
}

/// 一次「自由文本回答」的问题。
fn free_question(id: &str, question: &str) -> QuestionItem {
    QuestionItem {
        id: id.to_string(),
        header: None,
        question: question.to_string(),
        detail: None,
        options: Vec::new(),
        multi_select: false,
        pick_directory: false,
        schedule_rule: false,
    }
}

/// 去空白后的可选文本；空字符串视为「没给」。
fn trim_opt(text: Option<String>) -> Option<String> {
    text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// 按 id 取自由文本回答（跳过 / 空视为没有）。
fn answer_text<'a>(answers: &'a [AnswerItem], id: &str) -> Option<&'a str> {
    answers
        .iter()
        .find(|answer| answer.id == id)
        .and_then(|answer| answer.custom.as_deref().map(str::trim))
        .filter(|text| !text.is_empty())
}

/// 按 id 取单选回答的选项 label。
fn answer_choice<'a>(answers: &'a [AnswerItem], id: &str) -> Option<&'a str> {
    answers
        .iter()
        .find(|answer| answer.id == id)
        .and_then(|answer| answer.selected.first().map(String::as_str))
}

/// `schedule_list`: 列出所有定时任务。
pub struct ScheduleList;

#[async_trait]
impl Tool for ScheduleList {
    fn name(&self) -> &str {
        "schedule_list"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: concat!(
                "List all scheduled tasks with their status, rule and next fire time. ",
                "Use the id from this list with schedule_view / schedule_edit."
            )
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        let Some(scheduler) = handle(ctx) else {
            return Ok(ToolOutput::error(unavailable()));
        };
        let tasks = scheduler.list();
        // 驱动状态放在最前面：它是「这些任务会不会真的跑」的答案，用户读到
        // 第一行就该知道，而不是翻到底部（ADR-0088 §3）。
        let mut lines = vec![qcode_i18n::trp(
            "core.driver.footer",
            &[ctx.driver.describe()],
        )];
        if tasks.is_empty() {
            lines.push(qcode_i18n::tr("core.schedule.list_empty").to_string());
            return Ok(ToolOutput::ok(lines.join("\n")));
        }
        lines.push(qcode_i18n::trp(
            "core.schedule.list_header",
            &[tasks.len().to_string()],
        ));
        for task in &tasks {
            lines.push(row(task));
        }
        Ok(ToolOutput::ok(lines.join("\n")))
    }
}

/// `schedule_view`: 查看一个定时任务的详情与运行记录。
pub struct ScheduleView;

#[derive(Deserialize)]
struct ViewArgs {
    id: String,
}

#[async_trait]
impl Tool for ScheduleView {
    fn name(&self) -> &str {
        "schedule_view"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: concat!(
                "Show one scheduled task in full: rule, target, approval timeout, ",
                "latest delivery and recent delivery log. "
            )
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Task id (from schedule_list)."}
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let Some(scheduler) = handle(ctx) else {
            return Ok(ToolOutput::error(unavailable()));
        };
        let args: ViewArgs =
            serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))?;
        match scheduler.find(&args.id) {
            Some(task) => Ok(ToolOutput::ok(format!(
                "{}\n\n{}",
                detail(&task),
                qcode_core::scheduler::driver_footer(ctx.driver),
            ))),
            None => Ok(ToolOutput::error(qcode_i18n::trp(
                "tool.schedule.error.not_found",
                &[args.id.as_str()],
            ))),
        }
    }
}

/// `schedule_edit`: 修改、暂停 / 恢复、立即执行或删除一个定时任务。
pub struct ScheduleEdit;

/// `approval_timeout_secs`：键缺失 = 不修改，`null` = 清除，数字 = 设置。
///
/// 普通 `Option<Option<u64>>` 会把 `null` 和缺失都解成 `None`（serde 的
/// `Option` 对 `null` 默认当 `None`）；配合 `#[serde(default)]`，键缺失时
/// 用默认值（不调用本函数），键存在时由本函数把 `null` 变成 `Some(None)`。
fn deserialize_clearable<'de, D>(deserializer: D) -> Result<Option<Option<u64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<u64> = serde::Deserialize::deserialize(deserializer)?;
    Ok(Some(value))
}

#[derive(Deserialize)]
struct EditArgs {
    id: String,
    /// edit（默认）/ pause / resume / run_now / delete。
    #[serde(default)]
    action: String,
    title: Option<String>,
    prompt: Option<String>,
    rule: Option<String>,
    target: Option<String>,
    overlap: Option<String>,
    /// `null` = 清除审批超时。
    #[serde(default, deserialize_with = "deserialize_clearable")]
    approval_timeout_secs: Option<Option<u64>>,
}

#[async_trait]
impl Tool for ScheduleEdit {
    fn name(&self) -> &str {
        "schedule_edit"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: concat!(
                "Modify or control a scheduled task. action edit (default) updates the ",
                "given fields; pause / resume / run_now / delete act on the whole task. "
            )
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Task id (from schedule_list)."},
                    "action": {
                        "type": "string",
                        "enum": ["edit", "pause", "resume", "run_now", "delete"],
                        "description": "edit = change fields (default); pause = pause; resume = resume; run_now = run once now; delete = delete."
                    },
                    "title": {"type": "string", "description": "New task name; omit to keep the current one."},
                    "prompt": {"type": "string", "description": "New reminder text; omit to keep the current one."},
                    "rule": {"type": "string", "description": "The new rule, same format as schedule_create's rule."},
                    "target": {"type": "string", "description": "New delivery target, same values as schedule_create's target (new_once / new / session:<id>); omit to keep the current one."},
                    "overlap": {"type": "string", "enum": ["queue", "skip"], "description": "New busy-session policy: queue = wait for the turn to end, skip = skip this delivery; omit to keep the current one."},
                    "approval_timeout_secs": {"type": "integer", "minimum": 1, "description": "New approval timeout in seconds; pass null to clear it; omit to keep the current one."}
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        // 同 create：任务定义级的元数据变更。
        ApprovalClass::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let Some(scheduler) = handle(ctx) else {
            return Ok(ToolOutput::error(unavailable()));
        };
        let args: EditArgs =
            serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))?;
        if args.id.trim().is_empty() {
            return Ok(ToolOutput::error(
                qcode_i18n::tr("tool.schedule.error.missing_id").to_string(),
            ));
        }
        let now = now_local();
        let result: Result<String, ToolError> = match args.action.as_str() {
            "pause" => scheduler
                .pause(&args.id)
                .map(|_| qcode_i18n::trp("tool.schedule.output.paused", &[args.id.as_str()]))
                .map_err(|error| ToolError::Invalid(error.to_string())),
            "resume" => scheduler
                .resume(&args.id, now)
                .map(|_| qcode_i18n::trp("tool.schedule.output.resumed", &[args.id.as_str()]))
                .map_err(|error| ToolError::Invalid(error.to_string())),
            "run_now" => scheduler
                .run_now(&args.id, now)
                .map(|_| qcode_i18n::trp("tool.schedule.output.ran", &[args.id.as_str()]))
                .map_err(|error| ToolError::Invalid(error.to_string())),
            "delete" => scheduler
                .delete(&args.id)
                .map(|_| qcode_i18n::trp("tool.schedule.output.deleted", &[args.id.as_str()]))
                .map_err(|error| ToolError::Invalid(error.to_string())),
            "edit" | "" => {
                let patch = EditTask {
                    title: args.title.map(|title| title.trim().to_string()),
                    prompt: args.prompt,
                    rule: args
                        .rule
                        .as_deref()
                        .map(parse_rule)
                        .transpose()
                        .map_err(|error| ToolError::Invalid(error.to_string()))?,
                    target: args.target.as_deref().and_then(parse_target),
                    overlap: args.overlap.as_deref().and_then(Overlap::parse),
                    approval_timeout: args
                        .approval_timeout_secs
                        .map(|secs| secs.map(Duration::from_secs)),
                };
                let task = scheduler
                    .edit(&args.id, patch, now)
                    .map_err(|error| ToolError::Invalid(error.to_string()))?;
                Ok(qcode_i18n::trp(
                    "tool.schedule.output.saved",
                    &[
                        task.title.as_str(),
                        task.id.as_str(),
                        detail(&task).as_str(),
                    ],
                ))
            }
            other => {
                return Ok(ToolOutput::error(qcode_i18n::trp(
                    "tool.schedule.error.unknown_action",
                    &[other],
                )));
            }
        };
        Ok(ToolOutput::ok(format!(
            "{}\n\n{}",
            result?,
            qcode_core::scheduler::driver_footer(ctx.driver),
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use qcode_core::event::ToolStatus;
    use qcode_core::question::{AnswerItem, QuestionAnswer, Questioner};
    use qcode_core::schedule::{DeliveryReceipt, Overlap, Rule, Target, TaskStatus};
    use qcode_core::scheduler::ScheduleError;
    use serde_json::json;
    use time::PrimitiveDateTime;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// 内存假服务：记录调用、按固定任务回显，便于断言工具的参数解析与输出。
    #[derive(Debug, Default)]
    struct FakeHandle {
        created: Mutex<Vec<(NewTask, PrimitiveDateTime)>>,
        edited: Mutex<Vec<(String, EditTask, PrimitiveDateTime)>>,
        actions: Mutex<Vec<(String, String)>>,
        titles: Mutex<Vec<(String, String)>>,
        tasks: Mutex<Vec<ScheduledTask>>,
        unavailable: bool,
    }

    fn sample_task(id: &str, title: &str) -> ScheduledTask {
        ScheduledTask {
            id: id.to_string(),
            title: title.to_string(),
            prompt: "remind me".to_string(),
            rule: Rule::Daily { hour: 9, minute: 0 },
            target: Target::NewOnce,
            overlap: Overlap::Queue,
            approval_timeout: None,
            status: TaskStatus::Active,
            origin: qcode_core::schedule::Origin::User,
            stakeholders: Vec::new(),
            last_receipt: None,
            delivery_log: Vec::new(),
            earlier_records_pruned: false,
            missed_windows: 0,
            reserved_session_id: None,
            next_fire_at: None,
            cwd: None,
        }
    }

    impl SchedulerHandle for FakeHandle {
        fn list(&self) -> Vec<ScheduledTask> {
            self.tasks.lock().expect("tasks lock").clone()
        }

        fn find(&self, id: &str) -> Option<ScheduledTask> {
            self.tasks
                .lock()
                .expect("tasks lock")
                .iter()
                .find(|task| task.id == id)
                .cloned()
        }

        fn create(
            &self,
            input: NewTask,
            now: PrimitiveDateTime,
        ) -> Result<ScheduledTask, ScheduleError> {
            if self.unavailable {
                return Err(ScheduleError::Invalid("store read-only".to_string()));
            }
            self.created
                .lock()
                .expect("created lock")
                .push((input.clone(), now));
            let mut task = sample_task("t-1", &input.title);
            // 保留传入的 target：工具按它判断「绑定到哪个会话」并写会话标题。
            task.target = input.target.clone();
            Ok(task)
        }

        fn edit(
            &self,
            id: &str,
            patch: EditTask,
            now: PrimitiveDateTime,
        ) -> Result<ScheduledTask, ScheduleError> {
            let title = patch.title.clone().unwrap_or_else(|| "x".to_string());
            self.edited
                .lock()
                .expect("edited lock")
                .push((id.to_string(), patch, now));
            Ok(sample_task(id, &title))
        }

        fn pause(&self, id: &str) -> Result<(), ScheduleError> {
            self.actions
                .lock()
                .expect("actions lock")
                .push((id.to_string(), "pause".to_string()));
            Ok(())
        }

        fn resume(&self, id: &str, _now: PrimitiveDateTime) -> Result<(), ScheduleError> {
            self.actions
                .lock()
                .expect("actions lock")
                .push((id.to_string(), "resume".to_string()));
            Ok(())
        }

        fn run_now(&self, id: &str, _now: PrimitiveDateTime) -> Result<(), ScheduleError> {
            self.actions
                .lock()
                .expect("actions lock")
                .push((id.to_string(), "run_now".to_string()));
            Ok(())
        }

        fn delete(&self, id: &str) -> Result<(), ScheduleError> {
            self.actions
                .lock()
                .expect("actions lock")
                .push((id.to_string(), "delete".to_string()));
            Ok(())
        }

        fn set_session_title_if_missing(
            &self,
            session_id: &str,
            title: &str,
        ) -> Result<(), ScheduleError> {
            self.titles
                .lock()
                .expect("titles lock")
                .push((session_id.to_string(), title.to_string()));
            Ok(())
        }
    }

    fn context_with(handle: Option<Arc<dyn SchedulerHandle>>) -> ToolContext {
        // I18N.md 不变量 5：测试进程钉 `zh-CN`，且不在单个测试里换语言。下面这些
        // 断言直接比对中文文案（`已创建定时任务 …`），而 `parse_target` 还要拿
        // 当前语言的选项 label 去匹配用户点选的结果——不钉语言两边都会漂。
        qcode_i18n::pin_for_tests();
        ToolContext::for_test(".").with_scheduler(handle)
    }

    /// 与 [`context_with`] 同形，但显式给出「谁在推动时钟」。ADR-0088 之后这两个
    /// 问题是分开的：有 handle 只说明能登记，能不能触发由 `driver` 回答。
    fn context_with_driver(
        handle: Option<Arc<dyn SchedulerHandle>>,
        driver: qcode_core::scheduler::DriverStatus,
    ) -> ToolContext {
        let mut ctx = context_with(handle);
        ctx.driver = driver;
        ctx
    }

    #[tokio::test]
    async fn create_parses_rule_and_target() {
        let fake = Arc::new(FakeHandle::default());
        let ctx = context_with(Some(fake.clone() as Arc<dyn SchedulerHandle>));
        let output = ScheduleCreate
            .execute(
                &ctx,
                json!({
                    "title": "  daily report  ",
                    "prompt": "写日报",
                    "rule": "daily 09:30",
                    "target": "new",
                    "approval_timeout_secs": 60,
                }),
            )
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Ok);
        assert!(output.output.contains("已创建定时任务 `daily report`"));
        let created = fake.created.lock().expect("created lock");
        let (input, _) = created.first().expect("created");
        assert_eq!(input.title, "daily report");
        assert_eq!(
            input.rule,
            Rule::Daily {
                hour: 9,
                minute: 30
            }
        );
        assert_eq!(input.target, Target::New);
        assert_eq!(input.approval_timeout, Some(Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn create_rejects_bad_input() {
        let ctx = context_with(Some(
            Arc::new(FakeHandle::default()) as Arc<dyn SchedulerHandle>
        ));
        // 非法规则。
        let output = ScheduleCreate
            .execute(
                &ctx,
                json!({"title": "x", "prompt": "y", "rule": "every 5"}),
            )
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Error);
        assert!(output.output.contains("at least 60"), "{}", output.output);
        // 空标题：headless 下工具直接说明缺了什么，让模型先向用户确认。
        let output = ScheduleCreate
            .execute(
                &ctx,
                json!({"title": "  ", "prompt": "y", "rule": "daily 09:00"}),
            )
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Error);
        assert!(output.output.contains("缺少必要信息"), "{}", output.output);
    }

    /// 一个返回固定答案批次的提问器，用于引擎外的工具测试（同 ask.rs 的 Fixed）。
    #[derive(Debug)]
    struct Fixed(QuestionAnswer);

    #[async_trait]
    impl Questioner for Fixed {
        async fn ask(
            &self,
            _request: &QuestionRequest,
            _cancel: CancellationToken,
        ) -> QuestionAnswer {
            self.0.clone()
        }
    }

    /// 缺的字段由工具自己一次问全（名称 / 内容 / 周期 / 工作目录 / 目标），
    /// 答案回填后照常创建——这是「在对话中创建」的唯一入口，不依赖模型先调
    /// ask_user。
    #[tokio::test]
    async fn create_asks_for_missing_fields_in_one_round() {
        let fake = Arc::new(FakeHandle::default());
        let questioner = Arc::new(Fixed(QuestionAnswer::Answered(vec![
            AnswerItem {
                id: "title".into(),
                selected: vec![],
                custom: Some("每日日报".into()),
            },
            AnswerItem {
                id: "prompt".into(),
                selected: vec![],
                custom: Some("写日报".into()),
            },
            AnswerItem {
                id: "rule".into(),
                selected: vec![],
                custom: Some("daily 09:30".into()),
            },
            AnswerItem {
                id: "cwd".into(),
                selected: vec![],
                custom: Some("D:\\work".into()),
            },
            AnswerItem {
                id: "target".into(),
                selected: vec!["每次新建一个会话".into()],
                custom: None,
            },
        ])));
        let ctx = context_with(Some(fake.clone() as Arc<dyn SchedulerHandle>))
            .with_questioner(Some(questioner));
        let output = ScheduleCreate
            .execute(&ctx, json!({}))
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Ok, "{}", output.output);
        let created = fake.created.lock().expect("created lock");
        let (input, _) = created.first().expect("created");
        assert_eq!(input.title, "每日日报");
        assert_eq!(input.prompt, "写日报");
        assert_eq!(
            input.rule,
            Rule::Daily {
                hour: 9,
                minute: 30
            }
        );
        assert_eq!(input.target, Target::New);
        assert_eq!(input.cwd.as_deref(), Some("D:\\work"));
    }

    /// 在对话中创建（new_once + 会话里）时绑定当前会话，并把任务名写为会话标题
    /// ——对话没起名时，任务名就是对话名。
    #[tokio::test]
    async fn create_names_the_bound_session() {
        let fake = Arc::new(FakeHandle::default());
        let mut ctx = context_with(Some(fake.clone() as Arc<dyn SchedulerHandle>));
        ctx.session_id = "s-42".to_string();
        let output = ScheduleCreate
            .execute(
                &ctx,
                json!({"title": "每日日报", "prompt": "写日报", "rule": "daily 09:30"}),
            )
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Ok, "{}", output.output);
        let created = fake.created.lock().expect("created lock");
        let (input, _) = created.first().expect("created");
        assert_eq!(input.target, Target::Session { id: "s-42".into() });
        let titles = fake.titles.lock().expect("titles lock");
        assert_eq!(titles.len(), 1, "{titles:?}");
        assert_eq!(titles[0].0, "s-42");
        assert_eq!(titles[0].1, "每日日报");
    }

    /// 这是 ADR-0088 的核心行为变化：handle 在、驱动不在 = **登记成功**，
    /// 而不是「服务不可用」。旧行为让用户读成"这里不支持"，其实只是没人在催时钟。
    #[tokio::test]
    async fn create_succeeds_without_a_driver_but_says_so() {
        use qcode_core::scheduler::DriverStatus;
        let fake = Arc::new(FakeHandle::default());
        let ctx = context_with_driver(
            Some(fake.clone() as Arc<dyn SchedulerHandle>),
            // `Gone`：没有锁、有心跳（ADR-0091 的「来过又走了」）。这是「登记成功
            // 但驱动不在」的典型形状。
            DriverStatus::Gone {
                at: qcode_core::schedule::at(2026, time::Month::June, 1, 9, 0),
            },
        );
        let output = ScheduleCreate
            .execute(
                &ctx,
                json!({"title": "daily report", "prompt": "写日报", "rule": "daily 09:30"}),
            )
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Ok, "{}", output.output);
        assert!(output.output.contains("已创建定时任务"));
        // 三件事必须都说清楚，否则这条成功提示会误导人。
        assert!(output.output.contains("驱动不在跑"), "{}", output.output);
        assert!(output.output.contains("退出进程才停"), "{}", output.output);
        assert!(output.output.contains("不补跑"), "{}", output.output);
    }

    /// 驱动在跑时也要明说，用户才知道"这条真的会跑"。
    #[tokio::test]
    async fn create_reports_a_running_driver() {
        use qcode_core::scheduler::DriverStatus;
        let fake = Arc::new(FakeHandle::default());
        let at = qcode_core::schedule::at(2026, time::Month::June, 1, 9, 0);
        let ctx = context_with_driver(
            Some(fake.clone() as Arc<dyn SchedulerHandle>),
            DriverStatus::Running { at },
        );
        let output = ScheduleCreate
            .execute(
                &ctx,
                json!({"title": "x", "prompt": "y", "rule": "daily 09:30"}),
            )
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Ok);
        assert!(output.output.contains("驱动：在跑"), "{}", output.output);
        assert!(!output.output.contains("驱动不在跑"), "{}", output.output);
    }

    /// `schedule_list` 把驱动状态放在第一行：它是"这些任务会不会真的跑"的答案。
    #[tokio::test]
    async fn list_leads_with_the_driver_status() {
        use qcode_core::scheduler::DriverStatus;
        let fake = Arc::new(FakeHandle::default());
        let ctx = context_with_driver(
            Some(fake.clone() as Arc<dyn SchedulerHandle>),
            DriverStatus::Unknown,
        );
        let output = ScheduleList
            .execute(&ctx, json!({}))
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Ok);
        let first = output.output.lines().next().expect("first line");
        assert!(first.starts_with("驱动："), "{first}");
        assert!(
            output.output.contains("当前没有定时任务"),
            "{}",
            output.output
        );
    }

    #[tokio::test]
    async fn unavailable_without_handle() {
        let ctx = context_with(None);
        let output = ScheduleCreate
            .execute(
                &ctx,
                json!({"title": "x", "prompt": "y", "rule": "daily 09:00"}),
            )
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Error);
        assert!(output.output.contains("不可用"), "{}", output.output);
    }

    #[tokio::test]
    async fn list_renders_rows() {
        let fake = Arc::new(FakeHandle::default());
        {
            let mut tasks = fake.tasks.lock().expect("tasks lock");
            tasks.push(sample_task("t-1", "日报"));
            tasks.push(sample_task("t-2", "周报"));
        }
        let ctx = context_with(Some(fake.clone() as Arc<dyn SchedulerHandle>));
        let output = ScheduleList
            .execute(&ctx, json!({}))
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Ok);
        assert!(output.output.contains("共 2 个定时任务"));
        assert!(output.output.contains("日报"));
        assert!(output.output.contains("daily 09:00"));
    }

    #[tokio::test]
    async fn view_shows_detail_and_log() {
        let fake = Arc::new(FakeHandle::default());
        let receipt = DeliveryReceipt {
            occurrence_at: PrimitiveDateTime::new(
                time::Date::from_calendar_date(2026, time::Month::November, 10).expect("date"),
                time::Time::from_hms(9, 0, 0).expect("time"),
            ),
            delivered_at: PrimitiveDateTime::new(
                time::Date::from_calendar_date(2026, time::Month::November, 10).expect("date"),
                time::Time::from_hms(9, 0, 5).expect("time"),
            ),
            message_id: "m-1".to_string(),
            target_session_id: Some("s-1".to_string()),
            prompt: "生成日报".to_string(),
        };
        let mut task = sample_task("t-1", "日报");
        task.last_receipt = Some(receipt.clone());
        task.delivery_log = vec![receipt];
        fake.tasks.lock().expect("tasks lock").push(task);
        let ctx = context_with(Some(fake.clone() as Arc<dyn SchedulerHandle>));
        let output = ScheduleView
            .execute(&ctx, json!({"id": "t-1"}))
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Ok);
        assert!(output.output.contains("运行记录：共 1 条"));
        assert!(output.output.contains("message m-1"));
        // 不存在的任务。
        let output = ScheduleView
            .execute(&ctx, json!({"id": "nope"}))
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Error);
        assert!(output.output.contains("不存在"));
    }

    #[tokio::test]
    async fn edit_actions_and_fields() {
        let fake = Arc::new(FakeHandle::default());
        let ctx = context_with(Some(fake.clone() as Arc<dyn SchedulerHandle>));
        for action in ["pause", "resume", "run_now", "delete"] {
            let output = ScheduleEdit
                .execute(&ctx, json!({"id": "t-1", "action": action}))
                .await
                .expect("execute");
            assert_eq!(output.status, ToolStatus::Ok, "{action}");
        }
        let kinds: Vec<String> = {
            let actions = fake.actions.lock().expect("actions lock");
            actions.iter().map(|(_, kind)| kind.clone()).collect()
        };
        assert_eq!(kinds, ["pause", "resume", "run_now", "delete"]);

        // 字段编辑：改标题 / 规则 / 目标 / 清除审批超时。
        let output = ScheduleEdit
            .execute(
                &ctx,
                json!({
                    "id": "t-1",
                    "title": "新标题",
                    "rule": "weekly mon 10:00",
                    "target": "session:s-9",
                    "approval_timeout_secs": null,
                }),
            )
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Ok, "{}", output.output);
        let edited = fake.edited.lock().expect("edited lock");
        let (id, patch, _) = edited.first().expect("edited");
        assert_eq!(id, "t-1");
        assert_eq!(patch.title.as_deref(), Some("新标题"));
        assert_eq!(
            patch.rule,
            Some(Rule::Weekly {
                weekdays: vec![1],
                weekday: None,
                hour: 10,
                minute: 0
            })
        );
        assert_eq!(patch.target, Some(Target::Session { id: "s-9".into() }));
        assert_eq!(patch.approval_timeout, Some(None));
    }
}

#[cfg(test)]
mod cwd_fallback_tests {
    use super::*;

    /// ADR-0090 §11：headless 下没有 `questioner`，`cwd` 留在 `None` 的任务到点会
    /// 回落到驱动进程的 workspace——一个用户看不见的目录。兜底把它写成这个回合的
    /// 工作目录，于是「存了什么」与「将来在哪跑」是同一个答案。
    #[tokio::test]
    async fn create_falls_back_to_the_turn_working_directory() {
        let data = tempfile::tempdir().expect("data dir");
        let root = tempfile::tempdir().expect("workspace");
        let store = qcode_core::store::SessionStore::new(data.path());
        let scheduler = std::sync::Arc::new(qcode_core::scheduler::Scheduler::new(store.clone()));

        // `for_test` builds a context with one primary root and **no questioner**,
        // which is exactly the headless shape.
        let ctx =
            qcode_core::tool::ToolContext::for_test(root.path()).with_scheduler(Some(scheduler));
        let output = ScheduleCreate
            .execute(
                &ctx,
                serde_json::json!({
                    "title": "no-cwd",
                    "prompt": "x",
                    "rule": "daily 09:00"
                }),
            )
            .await
            .expect("execute");
        assert!(
            !output.output.contains("不可用"),
            "the tool refused instead of registering: {}",
            output.output
        );

        let tasks = store.load_schedule_tasks();
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        let cwd = tasks[0]
            .cwd
            .clone()
            .expect("cwd must be written, not left null");
        let stored = std::path::PathBuf::from(&cwd)
            .canonicalize()
            .expect("canonical");
        assert_eq!(
            stored,
            root.path().canonicalize().expect("canonical"),
            "the fallback should be this turn's workspace"
        );
    }

    /// 显式给的 cwd 不许被兜底覆盖——新增的只是「最后剩下 None」那一种情况。
    #[tokio::test]
    async fn an_explicit_cwd_is_kept() {
        let data = tempfile::tempdir().expect("data dir");
        let root = tempfile::tempdir().expect("workspace");
        let explicit = tempfile::tempdir().expect("explicit");
        let store = qcode_core::store::SessionStore::new(data.path());
        let scheduler = std::sync::Arc::new(qcode_core::scheduler::Scheduler::new(store.clone()));

        let ctx =
            qcode_core::tool::ToolContext::for_test(root.path()).with_scheduler(Some(scheduler));
        ScheduleCreate
            .execute(
                &ctx,
                serde_json::json!({
                    "title": "explicit",
                    "prompt": "x",
                    "rule": "daily 09:00",
                    "cwd": explicit.path().display().to_string()
                }),
            )
            .await
            .expect("execute");

        let tasks = store.load_schedule_tasks();
        assert_eq!(
            tasks[0].cwd.as_deref(),
            Some(explicit.path().display().to_string().as_str())
        );
    }
}
