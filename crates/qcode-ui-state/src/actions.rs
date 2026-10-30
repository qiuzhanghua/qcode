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
            self.push_notice("steer: no turn is running".to_string());
            return None;
        }
        self.clear_composer();
        self.scroll = 0;
        Some(UiAction::Steer(text))
    }

    pub(crate) fn clear_composer(&mut self) {
        self.composer = Composer::new();
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
                    self.push_notice("usage: /unsteer <n> (see /steering)");
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
                    self.push_notice(format!("usage: {command} <n> (see /queue)"));
                    None
                }
            };
        }

        if command == "/rollback" {
            let turn = argument.parse::<u32>().map_err(|_| ()).ok();
            return match turn {
                Some(turn) => Some(UiAction::Rollback(turn)),
                None => {
                    self.push_notice("usage: /rollback <turn> (see /checkpoint)".to_string());
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
            self.push_notice(format!(
                "unknown command '{command}'; try /skills, /subagents, /exit, or /<skill-name>"
            ));
            return None;
        };

        if self.status.running {
            self.push_notice("a turn is already running; Esc cancels it before activating a skill");
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
    pub(crate) fn handle_subagents(&mut self, argument: &str) {
        if self.child_groups.is_empty() {
            self.push_notice("no sub-agent runs in this session");
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
            text.push_str("\n\nUse /subagents <n> to toggle one; Ctrl-B toggles the newest.");
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
                self.push_notice(format!("sub-agent group {index}: {state}"));
            }
            _ => self.push_notice(format!(
                "usage: /subagents [1..{}]",
                self.child_groups.len()
            )),
        }
    }

    pub(crate) fn show_skills(&mut self) {
        if self.skills.is_empty() {
            self.push_notice(
                "no skills discovered; run `qc skills list` to see the roots that were checked",
            );
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
        text.push_str("\n\nUse /skills <name> to show the full instructions.");
        self.push_notice(text);
    }

    pub(crate) fn show_skill(&mut self, name: &str) {
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name) else {
            self.push_notice(format!("unknown skill '{name}'; try /skills"));
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
