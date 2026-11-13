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
use qcode_core::question::{QuestionAnswer, QuestionIntent, QuestionItem, QuestionRequest};
use qcode_core::schedule::{
    MIN_DELIVERY_INTERVAL, Overlap, ScheduledTask, Target, now_local, parse_rule,
};
use qcode_core::scheduler::{EditTask, NewTask, SchedulerHandle};
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

/// 没有运行时驱动的会话：定时任务工具不可用。
const UNAVAILABLE: &str =
    "定时任务服务不可用：当前会话没有接入定时任务引擎（请在带定时任务的界面中创建）。";

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

/// `target` 参数：`new_once`（默认）/ `new` / `session:<id>`。
fn parse_target(text: &str) -> Option<Target> {
    let text = text.trim();
    if text.is_empty() || text == "new_once" || text == "new-once" {
        return Some(Target::NewOnce);
    }
    if text == "new" {
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
    format!(
        "- [{}] {}（id: {}）\n  规则：{} · 目标：{} · 下次触发：{}",
        task.status.as_str(),
        task.title,
        task.id,
        task.rule.describe(),
        task.target.describe(),
        fire_display(task),
    )
}

/// 单个任务的详情。
fn detail(task: &ScheduledTask) -> String {
    let mut lines = vec![
        format!("定时任务 {}（id: {}）", task.title, task.id),
        format!("状态：{}", task.status.as_str()),
        format!("规则：{}", task.rule.describe()),
        format!("目标：{}", task.target.describe()),
        format!("下次触发：{}", fire_display(task)),
        format!(
            "最小投递间隔：{}s（全局护栏，防止轰炸）",
            MIN_DELIVERY_INTERVAL.whole_seconds()
        ),
    ];
    if let Some(timeout) = task.approval_timeout {
        lines.push(format!("审批超时：{}s", timeout.as_secs()));
    }
    if let Some(cwd) = &task.cwd {
        lines.push(format!("工作目录：`{cwd}`"));
    }
    if let Some(session) = &task.reserved_session_id {
        lines.push(format!("预留会话：`{session}`"));
    }
    if let Some(receipt) = &task.last_receipt {
        lines.push(format!(
            "最近投递：{o} → {target}（message {m}）",
            o = receipt.occurrence_at,
            target = receipt.target_session_id.as_deref().unwrap_or("-"),
            m = receipt.message_id,
        ));
    }
    let count = task.delivery_log.len();
    lines.push(format!(
        "运行记录：共 {count} 条（保留最近 30 天 / 200 条）"
    ));
    for receipt in task.delivery_log.iter().rev().take(5) {
        lines.push(format!(
            "  - {o} 投递到 {target}（message {m}）：{p}",
            o = receipt.delivered_at,
            target = receipt.target_session_id.as_deref().unwrap_or("-"),
            m = receipt.message_id,
            p = receipt.prompt,
        ));
    }
    if task.earlier_records_pruned {
        lines.push("（更早的记录已被裁剪）".to_string());
    }
    lines.join("\n")
}

/// `schedule_create`: 按模型给的描述创建一条定时任务。
pub struct ScheduleCreate;

#[derive(Deserialize)]
struct CreateArgs {
    title: String,
    prompt: String,
    rule: String,
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
                "The task lives in the store and keeps running after this session closes. ",
                "When the user did not specify when the task runs (rule) or where it ",
                "delivers (target), ask the user to confirm those first — never invent a ",
                "default schedule such as \"every day at 08:00\". For the delivery target ",
                "the question is exactly two options: reuse one session for every delivery, ",
                "or create a new session each time — do not offer vague alternatives. ",
                "The working directory (`cwd`) is where the delivered turn runs: use an ",
                "absolute path the user named; when the model cannot tell it from the ",
                "conversation, omit `cwd` and the tool asks the user for it itself."
            )
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "短而可区分的任务名称。"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "到点后投递给会话的提醒内容（相当于一条用户消息）。"
                    },
                    "rule": {
                        "type": "string",
                        "description": "触发规则：cron <expr> / once YYYY-MM-DD HH:MM / every <秒>（≥60）/ daily HH:MM / weekly <星期列表> HH:MM（可多选，逗号分隔，如 1,3,5；0或sun=周日）/ monthly <日> HH:MM（日 1-31）。"
                    },
                    "target": {
                        "type": "string",
                        "description": "投递目标，二选一：new_once = 每次复用同一个会话（默认；在对话中创建时复用当前对话）；new = 每次新建一个会话。仅当用户点名某个具体会话时才用 session:<会话ID>。"
                    },
                    "overlap": {
                        "type": "string",
                        "enum": ["queue", "skip"],
                        "description": "目标会话忙碌时：queue=等回合结束再投（默认），skip=跳过本次。"
                    },
                    "approval_timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "投递回合的审批超时秒数；不传则一直等人工审批。"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "投递回合的工作目录（绝对路径）。用户明确给了就用；拿不准就省略，工具会主动向用户询问（用户可跳过用默认目录）。"
                    }
                },
                "required": ["title", "prompt", "rule"],
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
            return Ok(ToolOutput::error(UNAVAILABLE));
        };
        let args: CreateArgs =
            serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))?;
        let title = args.title.trim();
        if title.is_empty() {
            return Ok(ToolOutput::error("标题不能为空"));
        }
        if args.prompt.trim().is_empty() {
            return Ok(ToolOutput::error("内容不能为空"));
        }
        // 规则 / 目标 / overlap 的解析错误是模型可以修正的输入错误，作为工具
        // 结果返回（todo 工具同款约定），而不是引擎级失败。
        let rule = match parse_rule(&args.rule) {
            Ok(rule) => rule,
            Err(error) => return Ok(ToolOutput::error(format!("规则无效: {error}"))),
        };
        let target = match parse_target(&args.target) {
            Some(target) => target,
            None => {
                return Ok(ToolOutput::error(
                    "target 需为 new_once（每次复用会话）/ new（每次新建会话）/ session:<会话ID>"
                        .to_string(),
                ));
            }
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
                None => return Ok(ToolOutput::error("overlap 需为 queue / skip".to_string())),
            }
        };
        let approval_timeout = args.approval_timeout_secs.map(Duration::from_secs);

        // 工作目录：模型没给时工具主动询问用户（可跳过，用会话默认目录）。
        // 不依赖模型自觉调用 ask_user——描述里的引导可能被模型忽略。
        let mut cwd = args.cwd;
        if cwd.is_none()
            && let Some(questioner) = &ctx.questioner
        {
            let request = QuestionRequest {
                call_id: ctx.call_id.clone(),
                questions: vec![QuestionItem {
                    id: "cwd".to_string(),
                    header: None,
                    question: "请指定该定时任务的工作目录（绝对路径；可跳过，将使用会话默认目录）"
                        .to_string(),
                    detail: None,
                    options: Vec::new(),
                    multi_select: false,
                    pick_directory: true,
                }],
                intent: QuestionIntent::Confirm,
                timeout: None,
            };
            // 超时 / 取消：不追问，用默认目录创建——所以只认 `Answered`，
            // 其余分支本来也什么都不做。
            if let QuestionAnswer::Answered(answers) =
                questioner.ask(&request, ctx.cancel.clone()).await
                && let Some(item) = answers.first().filter(|item| !item.is_skipped())
                && let Some(text) = item
                    .custom
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
            {
                cwd = Some(text.to_string());
            }
        }

        let task = scheduler
            .create(
                NewTask {
                    title: title.to_string(),
                    prompt: args.prompt,
                    rule,
                    target,
                    overlap,
                    approval_timeout,
                    cwd,
                },
                now_local(),
            )
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        Ok(ToolOutput::ok(format!(
            "已创建定时任务 `{}`（id: {}）\n{}",
            task.title,
            task.id,
            detail(&task),
        )))
    }
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
            return Ok(ToolOutput::error(UNAVAILABLE));
        };
        let tasks = scheduler.list();
        if tasks.is_empty() {
            return Ok(ToolOutput::ok("当前没有定时任务。"));
        }
        let mut lines = vec![format!("共 {} 个定时任务：", tasks.len())];
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
                    "id": {"type": "string", "description": "任务 id（来自 schedule_list）。"}
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
            return Ok(ToolOutput::error(UNAVAILABLE));
        };
        let args: ViewArgs =
            serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))?;
        match scheduler.find(&args.id) {
            Some(task) => Ok(ToolOutput::ok(detail(&task))),
            None => Ok(ToolOutput::error(format!("定时任务 `{}` 不存在", args.id))),
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
                    "id": {"type": "string", "description": "任务 id（来自 schedule_list）。"},
                    "action": {
                        "type": "string",
                        "enum": ["edit", "pause", "resume", "run_now", "delete"],
                        "description": "edit=改字段（默认）；pause=暂停；resume=恢复；run_now=立即执行一次；delete=删除。"
                    },
                    "title": {"type": "string"},
                    "prompt": {"type": "string"},
                    "rule": {"type": "string", "description": "新规则，格式同 schedule_create 的 rule。"},
                    "target": {"type": "string"},
                    "overlap": {"type": "string", "enum": ["queue", "skip"]},
                    "approval_timeout_secs": {"type": "integer", "minimum": 1, "description": "传 null 清除审批超时。"}
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
            return Ok(ToolOutput::error(UNAVAILABLE));
        };
        let args: EditArgs =
            serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))?;
        if args.id.trim().is_empty() {
            return Ok(ToolOutput::error("缺少任务 id"));
        }
        let now = now_local();
        let result: Result<String, ToolError> = match args.action.as_str() {
            "pause" => scheduler
                .pause(&args.id)
                .map(|_| format!("已暂停定时任务 `{}`", args.id))
                .map_err(|error| ToolError::Invalid(error.to_string())),
            "resume" => scheduler
                .resume(&args.id, now)
                .map(|_| format!("已恢复定时任务 `{}`", args.id))
                .map_err(|error| ToolError::Invalid(error.to_string())),
            "run_now" => scheduler
                .run_now(&args.id, now)
                .map(|_| format!("已触发定时任务 `{}` 立即执行", args.id))
                .map_err(|error| ToolError::Invalid(error.to_string())),
            "delete" => scheduler
                .delete(&args.id)
                .map(|_| format!("已删除定时任务 `{}`", args.id))
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
                Ok(format!(
                    "已保存定时任务 `{}`（id: {}）\n{}",
                    task.title,
                    task.id,
                    detail(&task),
                ))
            }
            other => {
                return Ok(ToolOutput::error(format!(
                    "未知 action `{other}`，应为 edit / pause / resume / run_now / delete"
                )));
            }
        };
        Ok(ToolOutput::ok(result?))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use qcode_core::event::ToolStatus;
    use qcode_core::schedule::{DeliveryReceipt, Overlap, Rule, Target, TaskStatus};
    use qcode_core::scheduler::ScheduleError;
    use serde_json::json;
    use time::PrimitiveDateTime;

    use super::*;

    /// 内存假服务：记录调用、按固定任务回显，便于断言工具的参数解析与输出。
    #[derive(Debug, Default)]
    struct FakeHandle {
        created: Mutex<Vec<(NewTask, PrimitiveDateTime)>>,
        edited: Mutex<Vec<(String, EditTask, PrimitiveDateTime)>>,
        actions: Mutex<Vec<(String, String)>>,
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
            stakeholders: Vec::new(),
            last_receipt: None,
            delivery_log: Vec::new(),
            earlier_records_pruned: false,
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
            let task = sample_task("t-1", &input.title);
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
    }

    fn context_with(handle: Option<Arc<dyn SchedulerHandle>>) -> ToolContext {
        ToolContext::for_test(".").with_scheduler(handle)
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
        // 空标题。
        let output = ScheduleCreate
            .execute(
                &ctx,
                json!({"title": "  ", "prompt": "y", "rule": "daily 09:00"}),
            )
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Error);
        assert!(output.output.contains("标题不能为空"));
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
