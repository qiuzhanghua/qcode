use super::*;

impl SetupView {
    pub(crate) fn seed_table(
        &mut self,
        table: &'static TableSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = table.path.join(".");
        let mut rows = Vec::new();
        match table.path {
            ["providers"] => {
                let providers = self.config.providers.clone();
                for (name, provider) in &providers {
                    rows.push(self.provider_row(name, provider, window, cx));
                }
            }
            ["pricing"] => {
                let pricing = self.config.pricing.clone();
                for (model, price) in &pricing {
                    rows.push(self.price_row(model, price, window, cx));
                }
            }
            ["approval", "rules"] => {
                let rules = self.config.approval.rules.clone();
                for rule in &rules {
                    rows.push(self.rule_row(rule, window, cx));
                }
            }
            ["mcp", "servers"] => {
                let servers = self.config.mcp.servers.clone();
                for server in &servers {
                    rows.push(self.mcp_row(server, window, cx));
                }
            }
            _ => {}
        }
        if let Some(state) = self.tables.get_mut(&key) {
            state.rows = rows;
        }
    }

    pub(crate) fn provider_row(
        &mut self,
        name: &str,
        provider: &ProviderConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
        let mut row = self.row(
            [
                ("name", name.to_string()),
                ("base_url", provider.base_url.clone().unwrap_or_default()),
                (
                    "api_key_env",
                    provider.api_key_env.clone().unwrap_or_default(),
                ),
                (
                    "api_key_command",
                    provider
                        .api_key_command
                        .as_ref()
                        .map(|argv| argv.join("\n"))
                        .unwrap_or_default(),
                ),
                (
                    "default_model",
                    provider.default_model.clone().unwrap_or_default(),
                ),
                ("__key", String::new()),
            ],
            [("stream_usage", provider.stream_usage)],
            [],
            window,
            cx,
        );
        row.cells
            .insert("__key", secret_input(window, cx, "API key (Keychain)"));
        row
    }

    pub(crate) fn price_row(
        &mut self,
        model: &str,
        price: &qcode_core::cost::Pricing,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
        self.row(
            [
                ("model", model.to_string()),
                ("input_usd_per_mtok", price.input_usd_per_mtok.to_string()),
                ("output_usd_per_mtok", price.output_usd_per_mtok.to_string()),
                (
                    "cache_read_usd_per_mtok",
                    price.cache_read_usd_per_mtok.to_string(),
                ),
                (
                    "cache_write_usd_per_mtok",
                    price.cache_write_usd_per_mtok.to_string(),
                ),
            ],
            [],
            [],
            window,
            cx,
        )
    }

    pub(crate) fn rule_row(
        &mut self,
        rule: &Rule,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
        self.row(
            [
                ("id", rule.id.clone()),
                ("tool", rule.tool.clone().unwrap_or_default()),
                (
                    "command_prefix",
                    rule.command_prefix.clone().unwrap_or_default(),
                ),
                ("path_glob", rule.path_glob.clone().unwrap_or_default()),
            ],
            [],
            [
                ("action", action_name(&rule.action).to_string()),
                (
                    "scope",
                    rule.scope.map(scope_name).unwrap_or("session").to_string(),
                ),
            ],
            window,
            cx,
        )
    }

    pub(crate) fn mcp_row(
        &mut self,
        server: &qcode_mcp::McpServerConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
        let env = server
            .env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.row(
            [
                ("name", server.name.clone()),
                ("command", server.command.clone()),
                ("args", server.args.join("\n")),
                ("env", env),
                ("timeout_secs", server.timeout_secs.to_string()),
            ],
            [("enabled", server.enabled)],
            [],
            window,
            cx,
        )
    }

    pub(crate) fn row<const N: usize, const B: usize, const S: usize>(
        &mut self,
        cells: [(&'static str, String); N],
        bools: [(&'static str, bool); B],
        selects: [(&'static str, String); S],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
        self.next_row_id += 1;
        let id = self.next_row_id;
        RowState {
            id,
            cells: cells
                .into_iter()
                .map(|(key, value)| (key, input(window, cx, value, key)))
                .collect(),
            bools: bools.into_iter().collect(),
            selects: selects.into_iter().collect(),
        }
    }

    pub(crate) fn add_provider_preset(
        &mut self,
        preset: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, base_url, key_env, model) = match preset {
            "deepseek" => (
                "deepseek",
                "https://api.deepseek.com/v1",
                "DEEPSEEK_API_KEY",
                "deepseek-flash",
            ),
            "openai" => (
                "openai",
                "https://api.openai.com/v1",
                "OPENAI_API_KEY",
                "gpt-4.1",
            ),
            "anthropic" => (
                "anthropic",
                "https://api.anthropic.com",
                "ANTHROPIC_API_KEY",
                "claude-sonnet-4-5",
            ),
            "minimax" => (
                "minimax",
                "https://api.minimaxi.com/v1",
                "MINIMAX_API_KEY",
                "MiniMax-M3",
            ),
            "ollama" => ("ollama", "http://localhost:11434/v1", "", "llama3.2"),
            _ => return,
        };
        let row = self.row(
            [
                ("name", name.to_string()),
                ("base_url", base_url.to_string()),
                ("api_key_env", key_env.to_string()),
                ("api_key_command", String::new()),
                ("default_model", model.to_string()),
                ("__key", String::new()),
            ],
            [("stream_usage", true)],
            [],
            window,
            cx,
        );
        let mut row = row;
        row.cells
            .insert("__key", secret_input(window, cx, "API key (Keychain)"));
        if let Some(state) = self.tables.get_mut("providers") {
            state.rows.push(row);
        }
        if let Some(input) = self.inputs.get("default_provider") {
            input.update(cx, |input, cx| {
                input.set_value(name, window, cx);
                cx.notify();
            });
        }
        cx.notify();
    }

    pub(crate) fn add_row(
        &mut self,
        table: &'static TableSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = table.path.join(".");
        let Some(state) = self.tables.get_mut(&key) else {
            return;
        };
        self.next_row_id += 1;
        let row = RowState {
            id: self.next_row_id,
            cells: table
                .columns
                .iter()
                .filter(|column| !matches!(column.kind, FieldKind::Bool | FieldKind::Select(_)))
                .map(|column| (column.key, input(window, cx, String::new(), column.label)))
                .collect(),
            bools: table
                .columns
                .iter()
                .filter_map(|column| match column.kind {
                    FieldKind::Bool => Some((column.key, true)),
                    _ => None,
                })
                .collect(),
            selects: table
                .columns
                .iter()
                .filter_map(|column| match column.kind {
                    FieldKind::Select(options) => {
                        Some((column.key, options.first().unwrap_or(&"").to_string()))
                    }
                    _ => None,
                })
                .collect(),
        };
        let mut row = row;
        if table.path == ["providers"] {
            row.cells
                .insert("__key", input(window, cx, String::new(), "API key"));
        }
        state.rows.push(row);
        cx.notify();
    }

    pub(crate) fn remove_row(&mut self, table_key: &str, row_id: u64, cx: &mut Context<Self>) {
        if let Some(table) = self.tables.get_mut(table_key) {
            table.rows.retain(|row| row.id != row_id);
        }
        cx.notify();
    }

    pub(crate) fn store_provider_key(
        &mut self,
        row_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(table) = self.tables.get_mut("providers") else {
            return;
        };
        let Some(row) = table.rows.iter_mut().find(|row| row.id == row_id) else {
            return;
        };
        let name = row
            .cells
            .get("name")
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let key = row
            .cells
            .get("__key")
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if name.is_empty() || key.is_empty() {
            self.error = Some("provider name and API key are required".to_string());
            cx.notify();
            return;
        }
        match keychain_store(&name, &key) {
            Ok(argv) => {
                if let Some(command) = row.cells.get("api_key_command") {
                    command.update(cx, |input, cx| {
                        input.set_value(argv.join("\n"), window, cx);
                        cx.notify();
                    });
                }
                if let Some(secret) = row.cells.get("__key") {
                    secret.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                        cx.notify();
                    });
                }
                self.info = Some(format!(
                    "stored the key for {name} in the platform keychain"
                ));
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }
}
