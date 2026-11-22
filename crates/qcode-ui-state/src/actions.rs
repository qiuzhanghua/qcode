use crate::*;

impl App {
    pub(crate) fn answer(&mut self, allow: bool, session: bool) -> Option<UiAction> {
        let prompt = self.approval.take()?;
        self.restoring = true;
        Some(UiAction::Approve {
            call_id: prompt.call_id,
            allow,
            session,
        })
    }

    /// Enter: slash commands run locally, everything else goes to the model.
    pub(crate) fn submit(&mut self) -> Option<UiAction> {
        let text = self.composer.text();
        if text.trim().is_empty() {
            return None;
        }
        // Slash commands never reach the model, and stay usable mid-turn.
        if text.trim_start().starts_with('/') {
            self.clear_composer();
            self.scroll = 0;
            if let Some(action) = self.run_command(text.trim()) {
                return Some(action);
            }
            return None;
        }
        if self.status.running {
            // A follow-up while the turn runs: queue it (default) or steer it
            // into the next step (`[ui] busy_delivery = "steer"`).
            self.clear_composer();
            self.scroll = 0;
            return Some(if self.steer_on_send {
                UiAction::Steer(text)
            } else {
                UiAction::Queue(text)
            });
        }
        self.clear_composer();
        self.scroll = 0;
        self.status.running = true;
        Some(UiAction::Submit(text))
    }

    /// Ctrl-S while a turn runs: hand the draft to its next step boundary.
    pub(crate) fn steer(&mut self) -> Option<UiAction> {
        let text = self.composer.text();
        if text.trim().is_empty() {
            return None;
        }
        if !self.status.running {
            self.push_notice(qcode_i18n::tr("core.notice.steer_no_turn").into_owned());
            return None;
        }
        self.clear_composer();
        self.scroll = 0;
        Some(UiAction::Steer(text))
    }

    pub(crate) fn clear_composer(&mut self) {
        self.composer = Composer::new();
    }

    /// Frontends that own their own text field hand the raw line here.
    ///
    /// The command table lives in this crate on purpose: `/diff`'s output
    /// shape, `/steer N`'s numbering and `/<skill-name>` matching must have
    /// exactly one implementation, or two frontends will drift apart. The
    /// frontend's job is to *execute* the returned action.
    ///
    /// `None` means the command was handled locally (a notice was pushed) and
    /// the model saw nothing.
    pub fn command(&mut self, line: &str) -> Option<UiAction> {
        self.run_command(line.trim())
    }

    /// Slash commands are local UI actions; the model never sees them.
    /// `/skills` inspects, `/<skill-name>` activates a skill by submitting a
    /// user message that contains its instructions.
    pub(crate) fn run_command(&mut self, raw: &str) -> Option<UiAction> {
        let (command, argument) = match raw.split_once(char::is_whitespace) {
            Some((command, argument)) => (command, argument.trim()),
            None => (raw, ""),
        };

        if command == "/checkpoint" {
            return Some(UiAction::Checkpoints);
        }

        if command == "/queue" {
            return Some(UiAction::QueueList);
        }

        if command == "/steering" {
            return Some(UiAction::SteeringList);
        }

        if command == "/unsteer" {
            let position = argument.parse::<usize>().ok().filter(|n| *n > 0);
            return match position {
                Some(position) => Some(UiAction::SteeringUndo(position)),
                None => {
                    self.push_notice(qcode_i18n::tr("core.notice.unsteer_usage").into_owned());
                    None
                }
            };
        }

        if command == "/steer" || command == "/drop" {
            let position = argument.parse::<usize>().ok().filter(|n| *n > 0);
            return match position {
                Some(position) if command == "/steer" => Some(UiAction::QueueSteer(position)),
                Some(position) => Some(UiAction::QueueDrop(position)),
                None => {
                    self.push_notice(qcode_i18n::trp("core.notice.queue_usage", &[command]));
                    None
                }
            };
        }

        if command == "/continue" || command == "/wrap" {
            if self.status.running {
                self.push_notice(qcode_i18n::tr("core.notice.turn_busy_esc").into_owned());
                return None;
            }
            let Some(turn) = self.paused_turn() else {
                self.push_notice(qcode_i18n::tr("core.notice.nothing_paused").into_owned());
                return None;
            };
            if command == "/wrap" {
                self.status.running = true;
                return Some(UiAction::WrapUp);
            }
            // The transcript keeps the paused card; the continuation turn is
            // rendered from its own `continuation` marker, not from a user
            // bubble the user never typed.
            let _ = turn;
            self.status.running = true;
            return Some(UiAction::Continue);
        }

        if command == "/rollback" {
            let turn = argument.parse::<u32>().map_err(|_| ()).ok();
            return match turn {
                Some(turn) => Some(UiAction::Rollback(turn)),
                None => {
                    self.push_notice(qcode_i18n::tr("core.notice.rollback_usage").into_owned());
                    None
                }
            };
        }

        if command == "/diff" {
            self.handle_diff(argument);
            return None;
        }

        if command == "/daemon" {
            return match argument {
                "start" => Some(UiAction::Daemon(crate::DaemonVerb::Start)),
                "stop" => Some(UiAction::Daemon(crate::DaemonVerb::Stop)),
                // 不带参数也当查询：那是用户打字最少、也最可能想要的那个答案。
                "" | "status" => Some(UiAction::Daemon(crate::DaemonVerb::Status)),
                other => {
                    self.push_notice(qcode_i18n::trp("core.notice.daemon_usage", &[other]));
                    None
                }
            };
        }

        if command == "/subagents" {
            self.handle_subagents(argument);
            return None;
        }

        // Three spellings: /exit is the documented one, /quit is what vim users
        // type, /q is the shortest that does not collide with a skill name.
        if matches!(command, "/exit" | "/quit" | "/q") {
            return Some(UiAction::Quit);
        }

        if command == "/skills" {
            if argument.is_empty() {
                self.show_skills();
            } else {
                self.show_skill(argument);
            }
            return None;
        }

        let name = command.strip_prefix('/').unwrap_or_default();
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name) else {
            self.push_notice(qcode_i18n::trp("core.notice.unknown_command", &[command]));
            return None;
        };

        if self.status.running {
            self.push_notice(qcode_i18n::tr("core.notice.turn_busy_esc_skill").into_owned());
            return None;
        }

        let mut text = format!("[skill:{}]\n{}\n", skill.name, skill.body.trim());
        if argument.is_empty() {
            text.push_str("\nFollow the skill instructions above now.");
        } else {
            text.push('\n');
            text.push_str(argument);
        }
        self.status.running = true;
        Some(UiAction::Submit(text))
    }

    /// `/subagents` lists child groups; `/subagents N` folds/unfolds one.
    /// `/diff` lists the write calls in this session; `/diff <n>` prints the
    /// patch of one. Both are terminal-side conveniences — the model never
    /// sees the command.
    pub(crate) fn handle_diff(&mut self, argument: &str) {
        let calls: Vec<&ToolEntry> = self
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Tool(tool) if tool.diff.is_some() => Some(tool.as_ref()),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            self.push_notice(qcode_i18n::tr("core.notice.no_file_changes").into_owned());
            return;
        }

        if argument.is_empty() {
            let mut text = format!("file changes ({}):", calls.len());
            for (index, tool) in calls.iter().enumerate() {
                let diff = tool.diff.as_ref().expect("filtered above");
                let additions: u32 = diff.files.iter().map(|file| file.additions).sum();
                let deletions: u32 = diff.files.iter().map(|file| file.deletions).sum();
                text.push_str(&format!(
                    "\n  {}. {} · {} +{additions} -{deletions}",
                    index + 1,
                    tool.name,
                    tool.summary
                ));
            }
            text.push_str(&format!(
                "\n\n{}",
                qcode_i18n::tr("core.notice.diff_use_hint")
            ));
            self.push_notice(text);
            return;
        }

        match argument.parse::<usize>().ok() {
            Some(index) if index >= 1 && index <= calls.len() => {
                let tool = calls[index - 1];
                let diff = tool.diff.as_ref().expect("filtered above");
                let mut text = format!("{}\n", tool.summary);
                if diff.patch.is_empty() {
                    text.push_str(qcode_i18n::tr("core.notice.no_patch_text").as_ref());
                } else {
                    text.push_str(&diff.patch);
                }
                self.push_notice(text);
            }
            _ => self.push_notice(qcode_i18n::trp(
                "core.notice.diff_usage",
                &[calls.len().to_string().as_str()],
            )),
        }
    }

    pub(crate) fn handle_subagents(&mut self, argument: &str) {
        if self.child_groups.is_empty() {
            self.push_notice(qcode_i18n::tr("core.notice.no_subagents").into_owned());
            return;
        }

        if argument.is_empty() {
            let groups = self.child_groups.clone();
            let mut text = format!("sub-agent groups ({}):", groups.len());
            for (index, group) in groups.iter().enumerate() {
                let state = if self.child_group_folded(group) {
                    "folded"
                } else {
                    "shown"
                };
                text.push_str(&format!(
                    "\n  {}. {} · {} nested · {state}",
                    index + 1,
                    short_id(group),
                    self.child_block_count(group),
                ));
            }
            text.push_str(&format!(
                "\n\n{}",
                qcode_i18n::tr("core.notice.subagents_use_hint")
            ));
            self.push_notice(text);
            return;
        }

        let parsed = argument.parse::<usize>().ok();
        match parsed {
            Some(index) if index >= 1 && index <= self.child_groups.len() => {
                let group = self.child_groups[index - 1].clone();
                self.toggle_child_group(&group);
                let state = if self.child_group_folded(&group) {
                    "folded"
                } else {
                    "shown"
                };
                self.push_notice(qcode_i18n::trp(
                    "core.notice.skill_group",
                    &[index.to_string().as_str(), state],
                ));
            }
            _ => self.push_notice(qcode_i18n::trp(
                "core.notice.subagents_usage",
                &[self.child_groups.len().to_string().as_str()],
            )),
        }
    }

    pub(crate) fn show_skills(&mut self) {
        if self.skills.is_empty() {
            self.push_notice(qcode_i18n::tr("core.notice.no_skills").into_owned());
            return;
        }

        let mut text = format!("skills ({}):", self.skills.len());
        for skill in &self.skills {
            text.push_str(&format!(
                "\n  {} [{}] files={}\n    {}",
                skill.name,
                skill.source,
                skill.files.len(),
                skill.description.replace(['\n', '\r'], " ")
            ));
        }
        text.push_str(&format!(
            "\n\n{}",
            qcode_i18n::tr("core.notice.skills_use_hint")
        ));
        self.push_notice(text);
    }

    pub(crate) fn show_skill(&mut self, name: &str) {
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name) else {
            self.push_notice(qcode_i18n::trp("core.notice.unknown_skill", &[name]));
            return;
        };

        let mut text = format!(
            "# Skill: {}\nsource: {} ({})\n",
            skill.name, skill.source, skill.dir
        );
        if !skill.files.is_empty() {
            text.push_str(&format!("files: {}", skill.files.join(", ")));
            if skill.files_truncated {
                text.push_str(" …");
            }
            text.push('\n');
        }
        text.push('\n');
        text.push_str(skill.body.trim());
        self.push_process(Block::Skill(text));
    }
}
